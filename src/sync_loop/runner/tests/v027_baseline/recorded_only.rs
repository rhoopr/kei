use super::{baseline_contract, pair};
use crate::icloud::photos::PhotoLibrary;
use crate::state::SqliteStateDb;
use serde_json::json;

tokio::task_local! { static ENABLED: bool; }

pub(super) fn enabled() -> bool {
    ENABLED.try_with(|value| *value).unwrap_or(false)
}

pub(super) async fn seed(library: &PhotoLibrary, db: &SqliteStateDb) {
    if !enabled() {
        return;
    }
    let album = library.all();
    let (capture, scope, zone) = album.catalog_work_scope().unwrap().unwrap();
    let records = pair(
        "old-only-unadmitted",
        "obsolete.JPG",
        "https://invalid.example/never",
        9,
    );
    let raw = serde_json::to_vec(&json!({"zones":[{"zoneID":zone,"records":records,
        "syncToken":"legacy-source-successor","moreComing":false}]}))
    .unwrap();
    capture
        .capture(
            crate::icloud::photos::catalog_observed_page(raw, &scope, "legacy-source").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        db.acquire_lock("verify real projected legacy source")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_catalog_records WHERE kind='asset'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
}

pub(super) fn assert_unadmitted(db: &SqliteStateDb) {
    if !enabled() {
        return;
    }
    let conn = db.acquire_lock("legacy source remains inert").unwrap();
    for table in [
        "provider_work_receipts",
        "provider_work_obligations",
        "provider_work_retries",
    ] {
        assert_eq!(
            conn.query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM provider_catalog_records WHERE kind='asset'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn v027_old_unadmitted_catalog_baseline_then_quiet_reopens_and_config_change() {
    ENABLED
        .scope(
            true,
            baseline_contract("private", false, false, false, false, false, false),
        )
        .await;
}

#[tokio::test]
async fn v027_inert_old_catalog_preserves_actual_failed_transfer_quiet_retry() {
    ENABLED
        .scope(
            true,
            baseline_contract("private", false, false, true, false, false, false),
        )
        .await;
}

#[derive(Clone)]
struct GatedBytes {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    allow: std::sync::Arc<std::sync::atomic::AtomicBool>,
    bytes: std::sync::Arc<Vec<u8>>,
}

impl wiremock::Respond for GatedBytes {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        use std::sync::atomic::Ordering;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.allow.load(Ordering::SeqCst) {
            wiremock::ResponseTemplate::new(200).set_body_bytes(self.bytes.as_slice())
        } else {
            wiremock::ResponseTemplate::new(500)
        }
    }
}

fn media_checksum() -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(super::MEDIA))
}

fn media_pair(zone: &str, id: &str, filename: &str, url: &str) -> Vec<serde_json::Value> {
    use base64::Engine as _;
    let checksum = media_checksum();
    let mut page = super::full_album_page_with_download(
        zone,
        id,
        "inventory-last-page",
        url,
        super::MEDIA.len() as u64,
        &checksum,
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] =
        json!(base64::engine::general_purpose::STANDARD.encode(filename));
    page["records"][0]["recordChangeTag"] = json!(format!("ct-master-{id}"));
    page["records"][1]["recordChangeTag"] = json!(format!("ct-asset-{id}"));
    page["records"].as_array().unwrap().clone()
}

fn missing_master_asset(zone: &str, id: &str, missing_master: &str) -> serde_json::Value {
    let mut asset = media_pair(zone, id, "orphan.JPG", "https://invalid.example/orphan")
        .pop()
        .unwrap();
    asset["fields"]["masterRef"]["value"]["recordName"] = json!(missing_master);
    asset
}

fn master_record(zone: &str, id: &str, record_name: &str) -> serde_json::Value {
    let mut master = media_pair(zone, id, "orphan.JPG", "https://invalid.example/orphan").remove(0);
    master["recordName"] = json!(record_name);
    master
}

#[cfg(test)]
async fn stock36_library_state(
    db: std::sync::Arc<SqliteStateDb>,
    zone_name: &'static str,
    inventory: Vec<serde_json::Value>,
    full_queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    changes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> crate::sync_cycle::LibraryState {
    use std::sync::Arc;

    use super::{
        ResetCycleSession, baseline_owner, make_run_cycle_library_state_with_passes, sync_token_key,
    };
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoLibrary;

    let session = ResetCycleSession {
        zone: zone_name,
        inventory: Arc::new(inventory),
        full_queries,
        changes,
        fail_full: false,
        auth_expired: false,
        boundary: "inventory-last-page",
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":zone_name,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db,
        baseline_owner(),
        "com",
    ));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected =
        make_run_cycle_library_state_with_passes(zone_name, &sync_token_key(zone_name), passes);
    selected.library = library;
    selected
}

async fn run_stock36_cycle(
    db: &std::sync::Arc<SqliteStateDb>,
    selected: &[&crate::sync_cycle::LibraryState],
    root: &std::path::Path,
    shared: &crate::auth::SharedSession,
    config: &crate::config::Config,
) -> anyhow::Result<crate::sync_cycle::CycleResult> {
    use super::make_run_cycle_download_config_builder;
    use crate::download;
    use tokio_util::sync::CancellationToken;

    let builder = make_run_cycle_download_config_builder(root, db.clone());
    crate::sync_cycle::run_cycle(
        selected,
        config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        shared,
        &CancellationToken::new(),
    )
    .await
}

fn epoch_state(db: &SqliteStateDb, zone: &str) -> String {
    let conn = db.acquire_lock("read epoch state").unwrap();
    conn.query_row(
        "SELECT json_extract(value,'$.state') FROM metadata WHERE key=?1",
        [crate::state::db::sync_epoch::library_key(zone)],
        |row| row.get(0),
    )
    .unwrap()
}

fn epoch_next_attempt(db: &SqliteStateDb, zone: &str) -> i64 {
    let conn = db.acquire_lock("read epoch deadline").unwrap();
    conn.query_row(
        "SELECT json_extract(value,'$.next_attempt_at') FROM metadata WHERE key=?1",
        [crate::state::db::sync_epoch::library_key(zone)],
        |row| row.get(0),
    )
    .unwrap()
}

fn token(db: &SqliteStateDb, zone: &str) -> Option<String> {
    let conn = db.acquire_lock("read zone token").unwrap();
    conn.query_row(
        "SELECT value FROM metadata WHERE key=?1",
        [super::sync_token_key(zone)],
        |row| row.get(0),
    )
    .ok()
}

fn destination_state(db: &SqliteStateDb, child: &str) -> (i64, i64) {
    let conn = db.acquire_lock("read destination state").unwrap();
    conn.query_row(
        "SELECT w.admitted,w.verified_media FROM provider_active_destinations w WHERE w.child=?1",
        [child],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn decision_state(db: &SqliteStateDb, child: &str) -> (String, i64, i64, i64) {
    let conn = db.acquire_lock("read decision state").unwrap();
    conn.query_row(
        "SELECT d.reason,d.attempts,d.next_retry_at,w.verified_media FROM provider_active_decisions d JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child WHERE d.child=?1",
        [child],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .unwrap()
}

fn generation_count(db: &SqliteStateDb) -> i64 {
    let conn = db.acquire_lock("count generations").unwrap();
    conn.query_row(
        "SELECT count(*) FROM provider_active_generations",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn downloaded_path(db: &SqliteStateDb, zone: &str, child: &str) -> std::path::PathBuf {
    let conn = db.acquire_lock("read downloaded path").unwrap();
    let path: String = conn
        .query_row(
            "SELECT local_path FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size='original'",
            [zone, child],
            |row| row.get(0),
        )
        .unwrap();
    std::path::PathBuf::from(path)
}

/// Current-accounting positive control: a validated first baseline may publish
/// its token while an admitted failed transfer remains durably queued. The
/// aggregate database checkpoint still stays held by the real failed count.
#[tokio::test]
async fn stock36_only_failed_media_keeps_validated_baseline_token_accounted() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_shared_session_for_run_cycle,
        seed_existing_upgrade_db,
    };
    use crate::state::SqliteStateDb;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);

    let calls = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow: Arc::clone(&allow),
            bytes: Arc::new(MEDIA.to_vec()),
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/progress"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;

    let mut inventory = media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    inventory.extend(media_pair(
        ZONE,
        "progress",
        "progress.JPG",
        &format!("{}/progress", server.uri()),
    ));

    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        inventory.clone(),
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;

    let first = run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .expect("ordinary first baseline must return a result");

    let first_state = epoch_state(&db, ZONE);
    let first_token = token(&db, ZONE);
    let first_decision = decision_state(&db, "asset-fail");
    let progress = destination_state(&db, "asset-progress");
    eprintln!(
        "L7 positive first cycle: failed={} full_queries={} epoch={first_state} token={first_token:?} fail={first_decision:?} progress={progress:?} calls={} db_advance={}",
        first.failed_count,
        full_queries.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
        first.can_advance_database_checkpoint(),
    );

    assert_eq!(first_state, "complete");
    assert!(first_token.is_some());
    assert!(first.failed_count > 0);
    assert!(
        !first.can_advance_database_checkpoint(),
        "the aggregate database checkpoint stays held by the real failed count"
    );
    assert_eq!(first_decision.3, 0, "failed media stays unverified");
    assert_eq!(progress, (1, 1), "same-library legitimate media progressed");
    assert_eq!(
        std::fs::read(downloaded_path(&db, ZONE, "asset-progress")).unwrap(),
        MEDIA,
        "same-library legitimate bytes must be published"
    );
    assert_eq!(full_queries.load(Ordering::SeqCst), 1);

    allow.store(true, Ordering::SeqCst);
    drop(selected);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();

    let second = run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .expect("deadline-boundary replay must complete");

    eprintln!(
        "L7 positive deadline-boundary retry: failed={} full_queries={} epoch={} token={:?} fail={:?} calls={}",
        second.failed_count,
        full_queries.load(Ordering::SeqCst),
        epoch_state(&db, ZONE),
        token(&db, ZONE),
        decision_state(&db, "asset-fail"),
        calls.load(Ordering::SeqCst),
    );

    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "the deadline-boundary replay must not run a new inventory"
    );
    assert_eq!(destination_state(&db, "asset-fail"), (1, 1));
    assert!(calls.load(Ordering::SeqCst) >= 3);
    assert_eq!(epoch_state(&db, ZONE), "complete");
    assert!(token(&db, ZONE).is_some());
}

/// A missing current master independently holds the first baseline. A due
/// admitted media item must still retry without a new inventory or any
/// checkpoint/epoch promotion, while a same-library item and a peer library
/// make legitimate progress. This is a deadline-boundary control, not a
/// configured-watch cadence claim.
#[tokio::test]
#[ignore = "explicit slow held-epoch qualification; run with --ignored"]
async fn stock36_held_identity_baseline_replays_due_admitted_media_without_inventory_or_promotion()
{
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        MEDIA, baseline_owner, make_run_cycle_config, make_shared_session_for_run_cycle,
        seed_existing_upgrade_db,
    };
    use crate::state::SqliteStateDb;

    const SUPPORT: &str = "SharedSync-v027-support";
    const PEER: &str = "SharedSync-v027-peer";

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);

    let calls = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow: Arc::clone(&allow),
            bytes: Arc::new(MEDIA.to_vec()),
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/progress"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/peer"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;

    let mut inventory = media_pair(
        SUPPORT,
        "fail",
        "fail.JPG",
        &format!("{}/fail", server.uri()),
    );
    inventory.extend(media_pair(
        SUPPORT,
        "progress",
        "progress.JPG",
        &format!("{}/progress", server.uri()),
    ));
    inventory.push(missing_master_asset(
        SUPPORT,
        "orphan",
        "currently-absent-master",
    ));
    let peer_inventory = media_pair(PEER, "peer", "peer.JPG", &format!("{}/peer", server.uri()));

    let support_full = Arc::new(AtomicUsize::new(0));
    let support_changes = Arc::new(AtomicUsize::new(0));
    let peer_full = Arc::new(AtomicUsize::new(0));
    let peer_changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let support = stock36_library_state(
        db.clone(),
        SUPPORT,
        inventory.clone(),
        Arc::clone(&support_full),
        Arc::clone(&support_changes),
    )
    .await;
    let peer = stock36_library_state(
        db.clone(),
        PEER,
        peer_inventory,
        Arc::clone(&peer_full),
        Arc::clone(&peer_changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;

    let first = run_stock36_cycle(&db, &[&support, &peer], &root, &shared, &config)
        .await
        .expect("held first baseline must return a result");

    let first_support_state = epoch_state(&db, SUPPORT);
    let first_support_token = token(&db, SUPPORT);
    let first_support_epoch_next = epoch_next_attempt(&db, SUPPORT);
    let first_fail = decision_state(&db, "asset-fail");
    let first_progress = destination_state(&db, "asset-progress");
    let first_peer_state = epoch_state(&db, PEER);
    let first_peer_token = token(&db, PEER);
    let first_support_full = support_full.load(Ordering::SeqCst);
    eprintln!(
        "L7 held first cycle: failed={} support_epoch={first_support_state} support_token={first_support_token:?} support_epoch_next={first_support_epoch_next} fail={first_fail:?} progress={first_progress:?} support_full={first_support_full} support_blocked={} reason={:?} peer_epoch={first_peer_state} peer_token={first_peer_token:?} peer_full={}",
        first.failed_count,
        first.stats.sync_token_blocked,
        first.stats.sync_token_blocked_reason,
        peer_full.load(Ordering::SeqCst),
    );

    assert_eq!(first_support_state, "pending");
    assert!(
        first_support_token.is_none(),
        "the independent current identity hold must not publish a token"
    );
    assert!(first.stats.sync_token_blocked);
    assert_ne!(
        first.stats.sync_token_blocked_reason,
        Some("download_failed"),
        "the baseline hold is independent of the media failure"
    );
    assert_eq!(first_fail.3, 0, "failed media stays admitted unverified");
    assert_eq!(
        first_progress,
        (1, 1),
        "same-library legitimate media progressed"
    );
    assert_eq!(first_peer_state, "complete");
    assert!(
        first_peer_token.is_some(),
        "the peer library keeps its own checkpoint progress"
    );

    allow.store(true, Ordering::SeqCst);
    drop(support);
    drop(peer);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let support = stock36_library_state(
        db.clone(),
        SUPPORT,
        inventory,
        Arc::clone(&support_full),
        Arc::clone(&support_changes),
    )
    .await;
    let peer = stock36_library_state(
        db.clone(),
        PEER,
        media_pair(PEER, "peer", "peer.JPG", &format!("{}/peer", server.uri())),
        Arc::clone(&peer_full),
        Arc::clone(&peer_changes),
    )
    .await;
    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();
    assert!(
        epoch_next_attempt(&db, SUPPORT) > chrono::Utc::now().timestamp(),
        "the due item must be ahead of the future epoch deadline"
    );

    let second = run_stock36_cycle(&db, &[&support, &peer], &root, &shared, &config)
        .await
        .expect("deadline-boundary retry while held must return a result");

    let second_support_state = epoch_state(&db, SUPPORT);
    let second_support_token = token(&db, SUPPORT);
    let second_fail = decision_state(&db, "asset-fail");
    let second_peer_token = token(&db, PEER);
    eprintln!(
        "L7 held deadline-boundary retry: failed={} support_epoch={second_support_state} support_token={second_support_token:?} fail={second_fail:?} support_full={} peer_token={second_peer_token:?} peer_full={} calls={}",
        second.failed_count,
        support_full.load(Ordering::SeqCst),
        peer_full.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
    );

    assert_eq!(
        support_full.load(Ordering::SeqCst),
        first_support_full,
        "retrying admitted media must not run a new inventory"
    );
    assert_eq!(
        second_fail.3, 1,
        "the due admitted media item must retry while the baseline is held"
    );
    assert_eq!(
        second_support_token, None,
        "the retry must not publish checkpoint authority"
    );
    assert_eq!(
        second_support_state, "pending",
        "the retry must not promote the independently held epoch"
    );
    assert!(
        second_peer_token.is_some(),
        "the peer keeps its own checkpoint progress"
    );
}

/// A resolved current master gives the affected library its own fresh baseline
/// proof. Only that ordinary baseline may complete the epoch and publish the
/// token; the earlier media retry does not.
#[tokio::test]
async fn stock36_held_identity_baseline_promotes_after_own_fresh_proof() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        MEDIA, baseline_owner, make_run_cycle_config, make_shared_session_for_run_cycle,
        seed_existing_upgrade_db,
    };
    use crate::state::SqliteStateDb;

    const ZONE: &str = "SharedSync-v027-fresh-proof";

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);

    let calls = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow: Arc::clone(&allow),
            bytes: Arc::new(MEDIA.to_vec()),
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/progress"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;

    let mut held_inventory =
        media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    held_inventory.extend(media_pair(
        ZONE,
        "progress",
        "progress.JPG",
        &format!("{}/progress", server.uri()),
    ));
    held_inventory.push(missing_master_asset(
        ZONE,
        "orphan",
        "currently-absent-master",
    ));

    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let held = stock36_library_state(
        db.clone(),
        ZONE,
        held_inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;
    run_stock36_cycle(&db, &[&held], &root, &shared, &config)
        .await
        .expect("held first baseline must return a result");
    assert_eq!(epoch_state(&db, ZONE), "pending");
    assert!(token(&db, ZONE).is_none());

    allow.store(true, Ordering::SeqCst);
    let mut resolved_inventory =
        media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    resolved_inventory.extend(media_pair(
        ZONE,
        "progress",
        "progress.JPG",
        &format!("{}/progress", server.uri()),
    ));
    resolved_inventory.push(master_record(
        ZONE,
        "resolved-master",
        "currently-absent-master",
    ));
    resolved_inventory.push(missing_master_asset(
        ZONE,
        "orphan",
        "currently-absent-master",
    ));

    drop(held);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let resolved = stock36_library_state(
        db.clone(),
        ZONE,
        resolved_inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    // Deadline-boundary control for the epoch hold, separate from cadence.
    let now = chrono::Utc::now().timestamp();
    db.acquire_lock("advance held epoch to its deadline")
        .unwrap()
        .execute(
            "UPDATE metadata SET value=json_set(value,'$.next_attempt_at',?1) WHERE key=?2",
            rusqlite::params![
                now.saturating_sub(1),
                crate::state::db::sync_epoch::library_key(ZONE)
            ],
        )
        .unwrap();

    let promoted = run_stock36_cycle(&db, &[&resolved], &root, &shared, &config)
        .await
        .expect("resolved own baseline must complete");
    eprintln!(
        "L7 held baseline own proof: failed={} epoch={} token={:?} full_queries={} fail={:?} calls={}",
        promoted.failed_count,
        epoch_state(&db, ZONE),
        token(&db, ZONE),
        full_queries.load(Ordering::SeqCst),
        decision_state(&db, "asset-fail"),
        calls.load(Ordering::SeqCst),
    );

    assert_eq!(epoch_state(&db, ZONE), "complete");
    assert!(token(&db, ZONE).is_some());
    assert_eq!(destination_state(&db, "asset-fail"), (1, 1));
}

fn captured_from_identity(
    identity: &crate::state::db::provider_generations::SelectionRetryIdentity,
) -> crate::state::db::provider_generations::CapturedSelectionRetry {
    crate::state::db::provider_generations::CapturedSelectionRetry {
        generation: identity.generation.clone(),
        specification_hash: identity.specification_hash.clone(),
        pass_key: identity.pass_key.clone(),
        child: identity.child.clone(),
        state_id: identity.state_id.clone(),
        manifest_hash: identity.manifest_hash.clone(),
        admission_hash: identity.admission_hash.clone(),
        destination: identity.destinations[0].clone(),
    }
}

#[tokio::test]
async fn stock36_media_failure_backoff_and_window_fences() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_shared_session_for_run_cycle,
        seed_existing_upgrade_db,
    };
    use crate::state::SqliteStateDb;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let calls = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow: Arc::clone(&allow),
            bytes: Arc::new(MEDIA.to_vec()),
        })
        .mount(&server)
        .await;
    let inventory = media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;
    run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .expect("failed-media baseline must return a result");

    let identity = {
        let conn = db.acquire_lock("read failed media identity").unwrap();
        let (generation, key): (String, String) = conn
            .query_row(
                "SELECT generation,pass_key FROM provider_active_decisions WHERE child='asset-fail'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        (generation, key)
    };
    let identity = db
        .selection_retry_identity(
            baseline_owner(),
            identity.0.clone(),
            identity.1.clone(),
            "asset-fail".into(),
        )
        .await
        .unwrap()
        .unwrap();
    let captured = captured_from_identity(&identity);
    assert!(
        db.defer_selection_media_failure(baseline_owner(), captured.clone())
            .await
            .unwrap()
    );
    let (reason, attempts, deadline, verified) = decision_state(&db, "asset-fail");
    assert_eq!(reason, "download_failed");
    assert_eq!(
        attempts, 2,
        "the first cycle and the explicit backoff both charge"
    );
    assert!(deadline > chrono::Utc::now().timestamp());
    assert_eq!(verified, 0);
    assert!(
        !db.defer_selection_media_failure(baseline_owner(), captured)
            .await
            .unwrap(),
        "a stale captured identity must not charge another attempt"
    );
    assert_eq!(decision_state(&db, "asset-fail").1, 2);

    let window = db
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 10)
        .await
        .unwrap();
    assert!(window.identities.is_empty(), "future item is not due");

    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();
    let window = db
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 10)
        .await
        .unwrap();
    assert_eq!(window.identities.len(), 1);
    let due = &window.identities[0];
    assert!(
        !db.ack_due_admitted_media_window(
            baseline_owner(),
            identity.generation.clone(),
            Some("wrong-cursor".to_owned()),
            (due.pass_key.clone(), due.child.clone()),
        )
        .await
        .unwrap(),
        "CAS mismatch must refuse the cursor ack"
    );
    assert!(
        db.ack_due_admitted_media_window(
            baseline_owner(),
            identity.generation.clone(),
            window.cursor.clone(),
            (due.pass_key.clone(), due.child.clone()),
        )
        .await
        .unwrap()
    );
    let cursor: Option<String> = {
        let conn = db.acquire_lock("read acked cursor").unwrap();
        conn.query_row(
            "SELECT replay_after FROM provider_active_generations WHERE id=?1",
            [&identity.generation],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        cursor,
        Some(serde_json::to_string(&(due.pass_key.clone(), due.child.clone())).unwrap())
    );

    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();
    db.acquire_lock("exhaust asset attempts")
        .unwrap()
        .execute(
            "UPDATE assets SET download_attempts=11 WHERE library=?1 AND id='asset-fail'",
            [ZONE],
        )
        .unwrap();
    let exhausted = db
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 10)
        .await
        .unwrap();
    assert!(
        exhausted.identities.is_empty(),
        "asset max attempts must filter before provider work"
    );
    assert_eq!(full_queries.load(Ordering::SeqCst), 1);
    assert!(calls.load(Ordering::SeqCst) > 0);
}

/// A phase-1 failure that the same-cycle cleanup resolves is not media failure
/// evidence. It must leave no item retry deadline or confirmation reason.
#[tokio::test]
async fn stock36_cleanup_success_leaves_no_item_retry_backoff() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        FlakyBytes, MEDIA, ZONE, baseline_owner, make_run_cycle_config,
        make_shared_session_for_run_cycle, seed_existing_upgrade_db,
    };
    use crate::state::SqliteStateDb;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);

    let calls = Arc::new(AtomicUsize::new(0));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/recovered"))
        .respond_with(FlakyBytes {
            calls: Arc::clone(&calls),
            bytes: Arc::new(MEDIA.to_vec()),
            fail_first: 1,
        })
        .mount(&server)
        .await;

    let inventory = media_pair(
        ZONE,
        "recovered",
        "recovered.JPG",
        &format!("{}/recovered", server.uri()),
    );
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        inventory.clone(),
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;

    let result = run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .expect("cleaned-up first baseline must complete");

    let (reason, attempts, deadline, verified) = decision_state(&db, "asset-recovered");
    eprintln!(
        "L7 cleanup success: failed={} full_queries={} calls={} verified_media={verified} reason={reason} attempts={attempts} deadline={deadline}",
        result.failed_count,
        full_queries.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
    );

    assert_eq!(result.failed_count, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(verified, 1);
    assert_eq!(
        (reason.as_str(), attempts, deadline),
        ("", 0, 0),
        "a resolved cleanup failure must not leave item retry evidence"
    );

    drop(selected);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let quiet = run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .expect("quiet reopen must complete");
    let (reason, attempts, deadline, verified) = decision_state(&db, "asset-recovered");
    eprintln!(
        "L7 cleanup success quiet reopen: failed={} full_queries={} calls={} verified_media={verified} reason={reason} attempts={attempts} deadline={deadline}",
        quiet.failed_count,
        full_queries.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
    );
    assert_eq!(quiet.failed_count, 0);
    assert_eq!(full_queries.load(Ordering::SeqCst), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "verified media must not GET again"
    );
    assert_eq!(
        (reason.as_str(), attempts, deadline, verified),
        ("", 0, 0, 1),
        "quiet reopen must preserve the resolved media with no retry evidence"
    );
}

fn admission_hash_for(
    generation: &str,
    key: &str,
    child: &str,
    manifest_hash: &str,
    admission: &str,
    reason: &str,
    attempts: i64,
    deadline: i64,
) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&(
        generation,
        key,
        child,
        manifest_hash,
        admission,
        reason,
        attempts,
        deadline,
    ))
    .unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

/// Compiled unchanged-reader control for the proposed durable reason. The
/// injected row is hash-valid for its exact bytes; the unchanged reader must
/// refuse it and mutate nothing.
#[tokio::test]
async fn new_reader_accepts_injected_download_failed_reason_without_mutation() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_shared_session_for_run_cycle,
        seed_existing_upgrade_db,
    };
    use crate::state::SqliteStateDb;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);

    let calls = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow: Arc::clone(&allow),
            bytes: Arc::new(MEDIA.to_vec()),
        })
        .mount(&server)
        .await;

    let inventory = media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;
    run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .expect("failed-media baseline must return a result");

    let (
        generation,
        key,
        child,
        manifest_hash,
        admission,
        old_reason,
        attempts,
        deadline,
        stored_hash,
    ) = {
        let conn = db.acquire_lock("read injected decision").unwrap();
        conn.query_row(
            "SELECT generation,pass_key,child,manifest_hash,admission,reason,attempts,next_retry_at,admission_hash FROM provider_active_decisions WHERE child='asset-fail'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(
        admission_hash_for(
            &generation,
            &key,
            &child,
            &manifest_hash,
            &admission,
            &old_reason,
            attempts,
            deadline
        ),
        stored_hash,
        "the fixture must begin with a hash-valid recorded decision"
    );

    let before = {
        let conn = db.acquire_lock("snapshot injected rows").unwrap();
        conn.query_row(
            "SELECT json_array(hex(manifest),manifest_hash,outcome,admission,reason,attempts,next_retry_at,admission_hash) FROM provider_active_decisions WHERE child='asset-fail'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    let injected_attempts = attempts.saturating_add(1).max(1);
    let injected_deadline = chrono::Utc::now().timestamp() + 3600;
    let injected_hash = admission_hash_for(
        &generation,
        &key,
        &child,
        &manifest_hash,
        &admission,
        "download_failed",
        injected_attempts,
        injected_deadline,
    );
    assert_ne!(
        injected_hash, stored_hash,
        "the admission hash must bind the reason"
    );
    assert_ne!(
        injected_hash,
        admission_hash_for(
            &generation,
            &key,
            &child,
            &manifest_hash,
            &admission,
            "download_failed",
            injected_attempts.saturating_add(1),
            injected_deadline
        ),
        "the admission hash must bind attempts"
    );
    assert_ne!(
        injected_hash,
        admission_hash_for(
            &generation,
            &key,
            &child,
            &manifest_hash,
            &admission,
            "download_failed",
            injected_attempts,
            injected_deadline.saturating_add(1)
        ),
        "the admission hash must bind the deadline"
    );
    {
        let conn = db.acquire_lock("inject download_failed").unwrap();
        conn.execute(
            "UPDATE provider_active_decisions SET reason='download_failed',attempts=?1,next_retry_at=?2,admission_hash=?3 WHERE generation=?4 AND pass_key=?5 AND child=?6",
            rusqlite::params![injected_attempts, injected_deadline, injected_hash, generation, key, child],
        )
        .unwrap();
    }
    let after_injection = {
        let conn = db.acquire_lock("snapshot injected reason").unwrap();
        conn.query_row(
            "SELECT json_array(hex(manifest),manifest_hash,outcome,admission,reason,attempts,next_retry_at,admission_hash) FROM provider_active_decisions WHERE child='asset-fail'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    assert_ne!(before, after_injection);

    let read = db
        .selection_decision(
            baseline_owner(),
            generation.clone(),
            key.clone(),
            child.clone(),
        )
        .await;
    assert!(
        matches!(read, Ok(Some(_))),
        "the new reader must accept download_failed: {read:?}"
    );
    let after_read = {
        let conn = db.acquire_lock("snapshot read-only refusal").unwrap();
        conn.query_row(
            "SELECT json_array(hex(manifest),manifest_hash,outcome,admission,reason,attempts,next_retry_at,admission_hash) FROM provider_active_decisions WHERE child='asset-fail'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    eprintln!(
        "L7 old-reader control: injected_reason=download_failed hash_valid=true read={read:?} unchanged={} attempts={attempts} deadline={deadline}",
        after_injection == after_read
    );
    assert_eq!(
        after_injection, after_read,
        "the unchanged reader must not mutate the injected row"
    );
    assert!(injected_attempts > 0 && injected_deadline > 0);
}

/// The old decoder refusal must survive ordinary replay and two reopens. This
/// control makes the injected decision due, runs the ordinary cycle twice with
/// a reopen between, and proves no retirement, compaction or completion.
#[tokio::test]
async fn new_reader_accepts_and_preserves_download_failed_across_replay_and_two_reopens() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_shared_session_for_run_cycle,
        seed_existing_upgrade_db,
    };
    use crate::state::SqliteStateDb;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);

    let calls = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow: Arc::clone(&allow),
            bytes: Arc::new(MEDIA.to_vec()),
        })
        .mount(&server)
        .await;

    let inventory = media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        inventory.clone(),
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;
    run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .expect("failed-media baseline must return a result");

    let (generation, key, child, manifest_hash, admission, attempts, _old_deadline) = {
        let conn = db.acquire_lock("read replay control decision").unwrap();
        conn.query_row(
            "SELECT generation,pass_key,child,manifest_hash,admission,attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-fail'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .unwrap()
    };
    let injected_attempts = attempts.saturating_add(1).max(1);
    let injected_deadline = 1;
    let injected_hash = admission_hash_for(
        &generation,
        &key,
        &child,
        &manifest_hash,
        &admission,
        "download_failed",
        injected_attempts,
        injected_deadline,
    );
    {
        let conn = db.acquire_lock("inject due download_failed").unwrap();
        conn.execute(
            "UPDATE provider_active_decisions SET reason='download_failed',attempts=?1,next_retry_at=?2,admission_hash=?3 WHERE generation=?4 AND pass_key=?5 AND child=?6",
            rusqlite::params![injected_attempts, injected_deadline, injected_hash, generation, key, child],
        )
        .unwrap();
    }
    let snapshot = |db: &SqliteStateDb| -> (String, String, String, i64, i64, Option<String>, i64) {
        let conn = db.acquire_lock("snapshot replay control").unwrap();
        let decision: (String, String, String, i64, i64, Option<String>) = conn
            .query_row(
                "SELECT manifest_hash,admission_hash,reason,attempts,next_retry_at,compacted_hash FROM provider_active_decisions WHERE child='asset-fail'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .unwrap();
        let verified: i64 = conn
            .query_row(
                "SELECT verified_media FROM provider_active_destinations WHERE child='asset-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (
            decision.0, decision.1, decision.2, decision.3, decision.4, decision.5, verified,
        )
    };
    let injected = snapshot(db.as_ref());
    assert_eq!(injected.2, "download_failed");
    assert_eq!(injected.5, None);
    assert_eq!(injected.6, 0);
    let calls_before = calls.load(Ordering::SeqCst);
    let generations_before = generation_count(db.as_ref());
    let spec: crate::state::db::provider_generations::GenerationSpec = {
        let conn = db
            .acquire_lock("read invalid generation specification")
            .unwrap();
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT specification FROM provider_active_generations WHERE id=?1",
                [&generation],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    let begin = db
        .begin_selection_generation(
            baseline_owner(),
            spec,
            crate::state::db::provider_generations::MAX_GENERATION_BYTES,
        )
        .await;
    let generations_after_state = generation_count(db.as_ref());
    eprintln!(
        "L7 old-reader state boundary: begin={begin:?} generations {generations_before}->{generations_after_state} row={:?}",
        snapshot(db.as_ref())
    );
    assert!(
        begin.is_ok(),
        "the ordinary generation boundary must run without treating the unverified row as complete"
    );
    assert!(
        generations_after_state > generations_before,
        "begin_selection_generation must create the new root and exercise its compaction/retirement boundary"
    );
    assert_eq!(
        snapshot(db.as_ref()),
        injected,
        "begin/compaction/retirement must not mutate the invalid decision"
    );

    drop(selected);
    drop(db);
    for reopen in 0..2 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let selected = stock36_library_state(
            db.clone(),
            ZONE,
            inventory.clone(),
            Arc::clone(&full_queries),
            Arc::clone(&changes),
        )
        .await;
        let replay = run_stock36_cycle(&db, &[&selected], &root, &shared, &config).await;
        let generations = generation_count(db.as_ref());
        let current = snapshot(db.as_ref());
        eprintln!(
            "L7 new-reader replay reopen={reopen} result_ok={} calls={} generations={generations} row={current:?}",
            replay.is_ok(),
            calls.load(Ordering::SeqCst),
        );
        assert!(generations >= generations_after_state);
        assert!(
            calls.load(Ordering::SeqCst) > calls_before,
            "the accepted due item must be attempted"
        );
        assert!(
            replay.is_ok(),
            "the new reader must consume the reason without a fatal refusal: {replay:?}"
        );
        assert_eq!(current.2, "download_failed");
        assert!(current.5.is_none(), "ordinary replay must not compact it");
        assert_eq!(current.6, 0, "ordinary replay must not complete it");
    }
}
