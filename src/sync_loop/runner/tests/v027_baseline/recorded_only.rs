use super::{baseline_contract, pair};
use crate::icloud::photos::PhotoLibrary;
use crate::state::SqliteStateDb;
use serde_json::json;

fn resolved_unfiled_only() -> crate::selection::Selection {
    crate::selection::Selection {
        albums: crate::selection::AlbumSelector::None,
        ..crate::selection::Selection::default()
    }
}

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
    config.filters.selection = resolved_unfiled_only();

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
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 10, 64)
        .await
        .unwrap();
    assert!(window.identities.is_empty(), "future item is not due");

    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();
    let window = db
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 10, 64)
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
            0,
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
            0,
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
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 10, 64)
        .await
        .unwrap();
    assert!(
        exhausted.identities.is_empty(),
        "asset max attempts must filter before provider work"
    );
    let unlimited = db
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 0, 64)
        .await
        .unwrap();
    assert_eq!(
        unlimited.identities.len(),
        1,
        "zero asset cap means unlimited"
    );
    for _ in 2..32 {
        let current = db
            .selection_retry_identity(
                baseline_owner(),
                identity.generation.clone(),
                identity.pass_key.clone(),
                "asset-fail".into(),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            db.defer_selection_media_failure(baseline_owner(), captured_from_identity(&current))
                .await
                .unwrap()
        );
    }
    assert_eq!(decision_state(&db, "asset-fail").1, 32);
    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();
    let saturated = db
        .due_admitted_media_window(baseline_owner(), identity.generation.clone(), 0, 64)
        .await
        .unwrap();
    assert_eq!(
        saturated.identities.len(),
        1,
        "saturated decision attempts do not exhaust the media item"
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

#[tokio::test]
async fn retained_root_window_is_unique_and_rotates() {
    use std::sync::Arc;

    use super::{ZONE, baseline_owner};
    use crate::state::SqliteStateDb;
    use crate::state::db::provider_generations::GenerationSpec;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &baseline_owner())
            .await
            .unwrap(),
    );
    let zone = json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"});
    let scope = json!({
        "container":"com.apple.photos.cloud","database":"private",
        "environment":"production","format":1,"realm":"com","zone":zone
    })
    .to_string();
    let pass_scope = json!({
        "container_id":null,"cross_zone_sources":[],
        "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
        "name":"","obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
        "query_filter":null,"zone":zone
    });
    let profile = json!({
        "format":2,"coverage":"selected_downloads","excludes":[],
        "passes":[{"key":"a".repeat(64),"kind":"Unfiled","scope":pass_scope,"excludes":[]}]
    });
    let spec = GenerationSpec {
        format: 2,
        scope,
        zone,
        config_hash: "b".repeat(64),
        basis: "b".repeat(64),
        profile: profile.clone(),
        metadata_enabled: false,
        metadata_flags: 0,
    };
    let root1 = db
        .begin_selection_generation(
            baseline_owner(),
            spec.clone(),
            crate::state::db::provider_generations::MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    let keep = |root: &crate::state::db::provider_generations::ActiveGeneration| {
        db.acquire_lock("keep test root alive")
            .unwrap()
            .execute(
                "INSERT INTO provider_active_decisions(generation,pass_key,child,manifest,manifest_hash,outcome,admission,reason,attempts,next_retry_at,charged_bytes) VALUES(?1,'pass','child',x'5b5d','hash','deferred','deferred','test',1,?2,1)",
                rusqlite::params![root.id, chrono::Utc::now().timestamp() + 3600],
            )
            .unwrap();
    };
    keep(&root1);
    let root2 = db
        .begin_selection_generation(
            baseline_owner(),
            spec.clone(),
            crate::state::db::provider_generations::MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    keep(&root2);
    let root3 = db
        .begin_selection_generation(
            baseline_owner(),
            spec.clone(),
            crate::state::db::provider_generations::MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    keep(&root3);
    {
        let conn = db.acquire_lock("order root window").unwrap();
        for (id, order) in [(&root1.id, 1_i64), (&root2.id, 2_i64), (&root3.id, 3_i64)] {
            conn.execute(
                "UPDATE provider_active_generations SET last_replayed_at=?2 WHERE id=?1",
                rusqlite::params![id, order],
            )
            .unwrap();
        }
    }
    let roots = db
        .retained_selection_roots(
            baseline_owner(),
            vec![crate::state::db::provider_generations::RetryRootSource {
                scope: spec.scope.clone(),
                config_hash: spec.config_hash.clone(),
                excluded: root1.id.clone(),
                profile: Some(profile.clone()),
                legacy_hashes: Vec::new(),
            }],
            7,
        )
        .await
        .unwrap();
    let ids: Vec<_> = roots.iter().map(|(_, root, _)| root.id.clone()).collect();
    assert_eq!(ids, vec![root2.id.clone(), root3.id.clone()]);
    assert_eq!(
        ids.len(),
        ids.iter().collect::<std::collections::HashSet<_>>().len()
    );
    let rotated = db
        .retained_selection_roots(
            baseline_owner(),
            vec![crate::state::db::provider_generations::RetryRootSource {
                scope: spec.scope,
                config_hash: spec.config_hash,
                excluded: root2.id.clone(),
                profile: Some(profile),
                legacy_hashes: Vec::new(),
            }],
            7,
        )
        .await
        .unwrap();
    let rotated_ids: Vec<_> = rotated.iter().map(|(_, root, _)| root.id.clone()).collect();
    assert_eq!(rotated_ids, vec![root1.id.clone(), root3.id.clone()]);
    assert!(!rotated_ids.contains(&root2.id));
}

#[derive(Clone)]
struct AuthLookupSession {
    zone: &'static str,
    lookups: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeldLookupFault {
    Auth,
    State,
    Cancel,
}

#[derive(Clone)]
struct HeldLookupSession {
    inner: AuthLookupSession,
    fault: HeldLookupFault,
    cancel: tokio_util::sync::CancellationToken,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for HeldLookupSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/records/lookup?") && self.fault != HeldLookupFault::Auth {
            self.inner
                .lookups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fault == HeldLookupFault::Cancel {
                self.cancel.cancel();
                anyhow::bail!("synthetic confirmation interrupted");
            }
            return Err(crate::state::error::StateError::ProviderSelectionInvalid.into());
        }
        crate::icloud::photos::PhotosSession::post(&self.inner, url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct FailIfPolledSession {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    healthy: bool,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for FailIfPolledSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.healthy {
            let zone =
                json!({"zoneName":"SharedSync-auth-stop-peer","ownerRecordName":"_defaultOwner"});
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({"batch":[{"records":[{"fields":{"itemCount":{"value":0}}}]}]}));
            }
            if url.contains("/zones/list") {
                return Ok(
                    json!({"zones":[{"zoneID":zone,"syncToken":"peer-boundary","deleted":false}]}),
                );
            }
            if url.contains("/records/query?") {
                return Ok(json!({"records":[],"syncToken":"peer-inventory"}));
            }
            if url.contains("/changes/zone?") {
                return Ok(
                    json!({"zones":[{"zoneID":zone,"syncToken":"peer-after","records":[],"moreComing":false}]}),
                );
            }
        }
        anyhow::bail!("synthetic later-source provider failure")
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for AuthLookupSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&_body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({"batch":[{"records":[{"fields":{"itemCount":{"value":1}}}]}]}));
        }
        if url.contains("/records/lookup?") {
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Err(anyhow::Error::new(
                crate::icloud::photos::session::HttpStatusError {
                    status: 401,
                    url: url.into(),
                    retry_after: None,
                    body: None,
                },
            ));
        }
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},"syncToken":"inventory-last-page","deleted":false}]}),
            );
        }
        if url.contains("/records/query?") {
            return Ok(json!({"records":[],"syncToken":"inventory-last-page","moreComing":false}));
        }
        if url.contains("/changes/zone?") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},"records":[],"syncToken":"auth-after","moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected auth lookup endpoint {url}")
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn held_cycle_surfaces_admitted_retry_auth_without_ack_or_fake_failure() {
    exercise_held_cycle_retry_fault(HeldLookupFault::Auth).await;
}

#[tokio::test]
async fn held_cycle_surfaces_retry_state_error_without_ack_or_fake_media_failure() {
    exercise_held_cycle_retry_fault(HeldLookupFault::State).await;
}

#[tokio::test]
async fn held_cycle_cancellation_during_retry_stops_peer_without_ack_or_backoff() {
    exercise_held_cycle_retry_fault(HeldLookupFault::Cancel).await;
}

async fn exercise_held_cycle_retry_fault(fault: HeldLookupFault) {
    use super::{
        ZONE, baseline_owner, make_run_cycle_config, make_run_cycle_library_state_with_passes,
        make_shared_session_for_run_cycle, seed_existing_upgrade_db, sync_token_key,
    };
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoLibrary;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/fail"))
        .respond_with(wiremock::ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let mut inventory = media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    inventory.push(missing_master_asset(
        ZONE,
        "orphan",
        "currently-absent-master",
    ));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let selected =
        stock36_library_state(db.clone(), ZONE, inventory, full_queries.clone(), changes).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;
    config.filters.selection = resolved_unfiled_only();
    let first = run_stock36_cycle(&db, &[&selected], &root, &shared, &config)
        .await
        .unwrap();
    assert_eq!(
        epoch_state(&db, ZONE),
        "pending",
        "real sparse identity must hold the baseline: {first:?}"
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path() == "/fail"),
        "the first source transfer must be attempted: {first:?}"
    );
    assert!(epoch_next_attempt(&db, ZONE) > chrono::Utc::now().timestamp());
    let snapshot = |db: &SqliteStateDb| -> (String, String, i64, i64, Option<String>) {
        let conn = db.acquire_lock("snapshot held auth owner").unwrap();
        let decision: (String, String, i64, i64) = conn.query_row(
            "SELECT reason,admission_hash,attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-fail'",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).unwrap();
        let cursor = conn.query_row("SELECT replay_after FROM provider_active_generations WHERE id=(SELECT generation FROM provider_active_decisions WHERE child='asset-fail')", [], |row| row.get(0)).unwrap();
        (decision.0, decision.1, decision.2, decision.3, cursor)
    };
    let before = snapshot(&db);
    assert!(
        before.3 <= chrono::Utc::now().timestamp(),
        "the actual admitted owner must already be due: {before:?}"
    );
    let calls_before = full_queries.load(Ordering::SeqCst);
    drop(selected);
    drop(first);
    assert_eq!(
        Arc::strong_count(&db),
        1,
        "held cycle owners must close before retry"
    );
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    assert_eq!(
        snapshot(&db),
        before,
        "the actual admitted decision survives reopen"
    );
    let lookups = Arc::new(AtomicUsize::new(0));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(HeldLookupSession {
            inner: AuthLookupSession {
                zone: ZONE,
                lookups: lookups.clone(),
            },
            fault,
            cancel: shutdown.clone(),
        }),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    };
    let mut selected =
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), vec![pass]);
    selected.library = library;
    const PEER: &str = "SharedSync-auth-stop-peer";
    let peer_calls = Arc::new(AtomicUsize::new(0));
    let mut peer_library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(FailIfPolledSession {
            calls: peer_calls.clone(),
            healthy: fault == HeldLookupFault::State,
        }),
        Arc::new(json!({"zoneName":PEER,"ownerRecordName":"_defaultOwner"})),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    peer_library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let peer_pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: peer_library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    };
    let mut peer =
        make_run_cycle_library_state_with_passes(PEER, &sync_token_key(PEER), vec![peer_pass]);
    peer.library = peer_library;
    assert_eq!(peer_calls.load(Ordering::SeqCst), 0);
    let builder = super::make_run_cycle_download_config_builder(&root, db.clone());
    let selected_sources = vec![&selected, &peer];
    let retried = crate::sync_cycle::run_cycle(
        &selected_sources,
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        crate::download::DownloadControls::download_hidden(),
        &shared,
        &shutdown,
    )
    .await
    .unwrap();
    if fault == HeldLookupFault::State {
        assert!(
            peer_calls.load(Ordering::SeqCst) > 0,
            "a generic state error must not stop a healthy peer"
        );
    } else {
        assert_eq!(
            peer_calls.load(Ordering::SeqCst),
            0,
            "authentication or cancellation must stop later provider sources"
        );
    }
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        1,
        "the held retry must reach actual confirmation"
    );
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        calls_before,
        "no replacement inventory"
    );
    assert_eq!(
        snapshot(&db),
        before,
        "session failure must not ack or back off admitted work"
    );
    assert_eq!(epoch_state(&db, ZONE), "pending");
    assert!(token(&db, ZONE).is_none());
    if fault == HeldLookupFault::State {
        assert!(
            !retried.session_expired,
            "state failure is not authentication: {retried:?}"
        );
        assert_eq!(
            retried.failed_count, 1,
            "the operational error must be visible: {retried:?}"
        );
        assert_eq!(
            retried.stats.failed, 0,
            "confirmation error must not invent a media failure"
        );
        return;
    }
    if fault == HeldLookupFault::Cancel {
        assert!(shutdown.is_cancelled());
        assert!(!retried.session_expired);
        assert!(
            retried.stats.interrupted,
            "cancellation must remain visible: {retried:?}"
        );
        assert_eq!(retried.failed_count, 0);
        assert_eq!(retried.stats.failed, 0);
        return;
    }
    assert!(
        retried.session_expired,
        "held retry must preserve typed authentication: {retried:?}"
    );
    assert_eq!(
        retried.failed_count, 0,
        "confirmation auth is not a media failure"
    );
    let facts =
        crate::cycle_reporter::CycleFacts::from_cycle_result(&retried, std::time::Duration::ZERO);
    assert_eq!(
        facts.status,
        crate::cycle_reporter::CycleStatus::SessionExpired
    );
    assert!(crate::sync_loop::runner::one_shot_partial_error(&retried).is_none());
    let notifier = crate::notifications::Notifier::new(None);
    let report_path = dir.path().join("held-auth.json");
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: true,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&config),
            health_dir: dir.path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter.report_completed_cycle(&mut health, facts).await;
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
    assert_eq!(report["status"], "session_expired");
    assert_eq!(report["stats"]["failed"], 0);
    assert_eq!(health.last_error.as_deref(), Some("session expired"));
}

#[tokio::test]
async fn retry_function_preserves_auth_and_cancellation_without_ack_or_backoff() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_run_cycle_download_config_builder,
        make_shared_session_for_run_cycle, seed_existing_upgrade_db,
    };
    use crate::commands::{AlbumPass, PassKind};
    use crate::download;
    use crate::icloud::photos::PhotoLibrary;
    use crate::state::SqliteStateDb;
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let calls = Arc::new(AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow,
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
        .expect("failed baseline must create the due admitted decision");
    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();

    let lookups = Arc::new(AtomicUsize::new(0));
    let session = AuthLookupSession {
        zone: ZONE,
        lookups: Arc::clone(&lookups),
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let owner_config = (*base(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from(ZONE),
    ))
    .clone();
    let owner_config = Arc::new(owner_config);
    let controls = download::DownloadControls::download_hidden();
    let client = reqwest::Client::new();
    let snapshot = |db: &SqliteStateDb| -> (String, String, i64, i64, Option<String>) {
        let conn = db.acquire_lock("snapshot auth retry owner").unwrap();
        let decision: (String, String, i64, i64) = conn
            .query_row(
                "SELECT reason,admission_hash,attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-fail'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let cursor: Option<String> = conn
            .query_row(
                "SELECT replay_after FROM provider_active_generations WHERE id=(SELECT generation FROM provider_active_decisions WHERE child='asset-fail')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (decision.0, decision.1, decision.2, decision.3, cursor)
    };
    let before = snapshot(db.as_ref());
    let calls_before = calls.load(Ordering::SeqCst);

    let auth = crate::download::replay_due_admitted_selection(
        &client,
        &passes,
        Some(&resolved_unfiled_only()),
        None,
        owner_config.clone(),
        controls,
        CancellationToken::new(),
    )
    .await;
    assert!(
        auth.as_ref()
            .err()
            .is_some_and(|error| { crate::icloud::photos::session::is_session_error(error) }),
        "confirmation auth must surface as the existing session error: {auth:?}"
    );
    assert_eq!(lookups.load(Ordering::SeqCst), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before,
        "auth must not GET"
    );
    assert_eq!(
        snapshot(db.as_ref()),
        before,
        "auth must not ack or back off"
    );

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let quiet = crate::download::replay_due_admitted_selection(
        &client,
        &passes,
        Some(&resolved_unfiled_only()),
        None,
        owner_config,
        controls,
        cancelled,
    )
    .await
    .expect("cancellation must not become a provider error");
    assert!(quiet.is_none());
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        1,
        "cancellation must not lookup"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        calls_before,
        "cancellation must not GET"
    );
    assert_eq!(
        snapshot(db.as_ref()),
        before,
        "cancellation must not ack or back off"
    );
}

#[derive(Clone)]
struct CountingRetrySession {
    inner: super::ResetCycleSession,
    lookups: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for CountingRetrySession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/records/lookup?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            if let Some([record]) = request["records"].as_array().map(Vec::as_slice)
                && let Some(name) = record["recordName"].as_str()
            {
                self.lookups.lock().unwrap().push(name.to_owned());
            }
        }
        crate::icloud::photos::PhotosSession::post(&self.inner, url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct RefreshMutationSession {
    initial: super::ResetCycleSession,
    refreshed: super::ResetCycleSession,
    db: std::sync::Arc<SqliteStateDb>,
    first_gets: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    mutated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mutation: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for RefreshMutationSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        use std::sync::atomic::Ordering;
        if url.contains("/records/lookup?") && self.first_gets.load(Ordering::SeqCst) > 0 {
            if !self.mutated.swap(true, Ordering::SeqCst) {
                let (generation, pass): (String, String) = {
                    let conn = self.db.acquire_lock("find refresh owner")?;
                    conn.query_row("SELECT generation,pass_key FROM provider_active_decisions WHERE child='asset-refresh'", [], |r| Ok((r.get(0)?,r.get(1)?)))?
                };
                let before = self
                    .db
                    .selection_retry_identity(
                        super::baseline_owner(),
                        generation.clone(),
                        pass.clone(),
                        "asset-refresh".into(),
                    )
                    .await?
                    .unwrap();
                // A separately owned confirmation deferral lands after the first
                // failed transfer and before the refreshed retry dispatch.
                self.db
                    .defer_selection_confirmation(
                        super::baseline_owner(),
                        generation.clone(),
                        pass.clone(),
                        "asset-refresh".into(),
                    )
                    .await?;
                let after = self
                    .db
                    .selection_retry_identity(
                        super::baseline_owner(),
                        generation,
                        pass,
                        "asset-refresh".into(),
                    )
                    .await?
                    .unwrap();
                *self.mutation.lock().unwrap() =
                    Some((before.admission_hash, after.admission_hash));
            }
            return crate::icloud::photos::PhotosSession::post(&self.refreshed, url, body, headers)
                .await;
        }
        crate::icloud::photos::PhotosSession::post(&self.initial, url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct CountedGone(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl wiremock::Respond for CountedGone {
    fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        wiremock::ResponseTemplate::new(410)
    }
}

async fn admit_due_media(
    db: &SqliteStateDb,
    generation: &str,
    pass_key: &str,
    zone: &str,
    pair: &[serde_json::Value],
    path: &std::path::Path,
    attempts: u32,
) -> String {
    use super::baseline_owner;
    use crate::state::VersionSizeKey;
    use crate::state::db::provider_generations::{ActiveDecision, MAX_GENERATION_BYTES};
    use crate::state::db::provider_selection::{
        SelectionDecision, SelectionDestination, SelectionOutcome, SelectionPath,
    };

    let master_json = pair[0].clone();
    let asset_json = pair[1].clone();
    let child = asset_json["recordName"].as_str().unwrap().to_owned();
    let master = master_json["recordName"].as_str().unwrap().to_owned();
    let body = serde_json::to_vec(&json!({"records": [asset_json, master_json]})).unwrap();
    let photo = crate::icloud::photos::current_asset(
        &body,
        &child,
        &master,
        &json!({"zoneName": zone, "ownerRecordName": "_defaultOwner"}),
    )
    .unwrap();
    let version = photo
        .versions()
        .iter()
        .find(|(size, _)| VersionSizeKey::from(*size) == VersionSizeKey::Original)
        .map(|(_, facts)| facts)
        .unwrap();
    let metadata = photo.metadata_arc(VersionSizeKey::Original);
    let metadata_hash = metadata.metadata_hash.clone().unwrap();
    let destination = SelectionDestination {
        version_size: VersionSizeKey::Original.as_str().to_owned(),
        path: SelectionPath::from_path(path),
        checksum: version.checksum.to_string(),
        size: version.size,
        metadata_hash: metadata_hash.clone(),
    };
    let manifest = ActiveDecision {
        state_id: child.clone(),
        sources: vec![],
        decision: SelectionDecision {
            pass_key: pass_key.to_owned(),
            child: child.clone(),
            master: Some(master),
            confirmation: Some(body),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations: vec![destination],
        },
    };
    let record = crate::test_helpers::TestAssetRecord::new(&child)
        .library(zone)
        .version_size(VersionSizeKey::Original)
        .filename(path.file_name().unwrap().to_str().unwrap())
        .checksum(&version.checksum)
        .size(version.size)
        .created_at(photo.created())
        .added_at(photo.added_date())
        .metadata((*metadata).clone())
        .build();
    db.project_selection_decision_with_proof(
        baseline_owner(),
        generation.to_owned(),
        manifest,
        vec![record],
        None,
        MAX_GENERATION_BYTES,
    )
    .await
    .unwrap();
    for _ in 0..attempts {
        let identity = db
            .selection_retry_identity(
                baseline_owner(),
                generation.to_owned(),
                pass_key.to_owned(),
                child.clone(),
            )
            .await
            .unwrap()
            .unwrap();
        let captured = captured_from_identity(&identity);
        assert!(
            db.defer_selection_media_failure(baseline_owner(), captured)
                .await
                .unwrap()
        );
    }
    child
}

#[tokio::test]
async fn actual_retry_function_bounds_source_and_rotates_three_roots() {
    use std::sync::Arc;

    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_run_cycle_download_config_builder,
        make_shared_session_for_run_cycle, seed_existing_upgrade_db,
    };
    use crate::download;
    use crate::state::SqliteStateDb;
    use tokio_util::sync::CancellationToken;

    const EXTRA: usize = 64;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let allow = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/fail"))
        .respond_with(GatedBytes {
            calls: Arc::clone(&calls),
            allow: Arc::clone(&allow),
            bytes: Arc::new(MEDIA.to_vec()),
        })
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path_regex(r"^/extra-\d+$"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let fail_pair = media_pair(ZONE, "fail", "fail.JPG", &format!("{}/fail", server.uri()));
    let full_queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let first_library = stock36_library_state(
        db.clone(),
        ZONE,
        fail_pair.clone(),
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;
    run_stock36_cycle(&db, &[&first_library], &root, &shared, &config)
        .await
        .expect("first baseline must create the current root");

    let (current_generation, pass_key): (String, String) = {
        let conn = db.acquire_lock("read current root").unwrap();
        conn.query_row(
            "SELECT generation,pass_key FROM provider_active_decisions WHERE child='asset-fail'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    let spec: crate::state::db::provider_generations::GenerationSpec = {
        let conn = db.acquire_lock("read current spec").unwrap();
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT specification FROM provider_active_generations WHERE id=?1",
                [&current_generation],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    };
    async fn begin_fairness_root(
        db: &SqliteStateDb,
        spec: crate::state::db::provider_generations::GenerationSpec,
    ) -> crate::state::db::provider_generations::ActiveGeneration {
        db.begin_selection_generation(
            baseline_owner(),
            spec,
            crate::state::db::provider_generations::MAX_GENERATION_BYTES,
        )
        .await
        .unwrap()
    }

    let future_root = begin_fairness_root(&db, spec.clone()).await;
    let future_pair = media_pair(
        ZONE,
        "future",
        "future.JPG",
        &format!("{}/future", server.uri()),
    );
    admit_due_media(
        &db,
        &future_root.id,
        &pass_key,
        ZONE,
        &future_pair,
        &root.join("future.JPG"),
        1,
    )
    .await;

    // A real excluded decision makes the oldest scheduled source non-media.
    let excluded = media_pair(
        ZONE,
        "excluded",
        "excluded.JPG",
        "https://invalid.example/excluded",
    );
    let excluded_body = serde_json::to_vec(&json!({"records": excluded})).unwrap();
    db.project_selection_decision_with_proof(
        baseline_owner(),
        future_root.id.clone(),
        crate::state::db::provider_generations::ActiveDecision {
            state_id: "asset-excluded".into(),
            sources: vec![],
            decision: crate::state::db::provider_selection::SelectionDecision {
                pass_key: pass_key.clone(),
                child: "asset-excluded".into(),
                master: Some("excluded".into()),
                confirmation: Some(excluded_body),
                outcome: crate::state::db::provider_selection::SelectionOutcome::Excluded,
                reason: "filtered".into(),
                destinations: vec![],
            },
        },
        vec![],
        None,
        crate::state::db::provider_generations::MAX_GENERATION_BYTES,
    )
    .await
    .unwrap();

    let refused_root = begin_fairness_root(&db, spec.clone()).await;
    let wrong_correct = media_pair(
        ZONE,
        "wrong",
        "wrong.JPG",
        &format!("{}/wrong", server.uri()),
    );
    admit_due_media(
        &db,
        &refused_root.id,
        &pass_key,
        ZONE,
        &wrong_correct,
        &root.join("wrong.JPG"),
        0,
    )
    .await;

    let good_pair = media_pair(ZONE, "good", "good.JPG", &format!("{}/good", server.uri()));
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/good"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    admit_due_media(
        &db,
        &refused_root.id,
        &pass_key,
        ZONE,
        &good_pair,
        &root.join("good.JPG"),
        0,
    )
    .await;

    // Generation ordering uses millisecond timestamps. Separate the current
    // root from its historical peers and verify the production owner lookup.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let peer_root = begin_fairness_root(&db, spec.clone()).await;
    assert_eq!(
        db.current_download_generation(
            baseline_owner(),
            spec.scope.clone(),
            spec.config_hash.clone(),
        )
        .await
        .unwrap()
        .unwrap()
        .id,
        peer_root.id,
        "the 64-refusal root must be the actual current owner"
    );
    let mut extra_pairs = Vec::new();
    for index in 0..EXTRA {
        let id = format!("extra-{index:02}");
        let pair = media_pair(
            ZONE,
            &id,
            "fail.JPG",
            &format!("{}/extra-{index:02}", server.uri()),
        );
        admit_due_media(
            &db,
            &peer_root.id,
            &pass_key,
            ZONE,
            &pair,
            &root.join(format!("extra-{index:02}.JPG")),
            0,
        )
        .await;
        extra_pairs.push(pair);
    }
    for id in [&future_root.id, &current_generation, &refused_root.id] {
        db.rotate_retained_selection_root(baseline_owner(), id.clone())
            .await
            .unwrap();
    }
    let mut inventory = fail_pair.clone();
    for pair in &extra_pairs {
        // Current confirmation cannot prove a pair when the master is absent.
        // The originally admitted confirmation remains intact and still due.
        inventory.push(pair[1].clone());
    }
    // The current provider view for the wrong-proof child differs from its
    // admitted resource, so the retry owner refuses it without a GET.
    let mut wrong_current = wrong_correct.clone();
    wrong_current[1]["fields"]["masterRef"]["value"]["recordName"] = serde_json::json!("wrong");
    wrong_current[1]["fields"]["assetDate"]["value"] = serde_json::json!(1_600_000_000_000_i64);
    inventory.extend(wrong_current);
    inventory.extend(future_pair);
    inventory.extend(good_pair);
    let mut restored_inventory: Vec<_> = inventory
        .iter()
        .filter(|record| {
            !record["recordName"]
                .as_str()
                .is_some_and(|name| name.starts_with("asset-extra-"))
        })
        .cloned()
        .collect();
    for pair in &extra_pairs {
        restored_inventory.extend(pair.clone());
    }
    let generations_before = generation_count(&db);
    let epoch_before = epoch_state(&db, ZONE);
    let token_before = token(&db, ZONE);
    let future_before = decision_state(&db, "asset-future");
    let wrong_before = decision_state(&db, "asset-wrong");
    assert!(future_before.2 > chrono::Utc::now().timestamp());
    let lookups = Arc::new(std::sync::Mutex::new(Vec::new()));
    let session = CountingRetrySession {
        inner: super::ResetCycleSession {
            zone: ZONE,
            inventory: Arc::new(inventory),
            full_queries: Arc::clone(&full_queries),
            changes: Arc::clone(&changes),
            fail_full: false,
            auth_expired: false,
            boundary: "inventory-last-page",
        },
        lookups: Arc::clone(&lookups),
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let passes = [crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let passes = passes.as_slice();
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let owner_config = Arc::new(
        (*base(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from(ZONE),
        ))
        .clone(),
    );
    let controls = download::DownloadControls::download_hidden();
    let client = reqwest::Client::new();
    allow.store(true, std::sync::atomic::Ordering::SeqCst);
    let baseline_gets = calls.load(std::sync::atomic::Ordering::SeqCst);

    let first = crate::download::replay_due_admitted_selection(
        &client,
        passes,
        Some(&resolved_unfiled_only()),
        None,
        owner_config.clone(),
        controls,
        CancellationToken::new(),
    )
    .await
    .expect("fairness wake must complete");
    let first_lookups = lookups.lock().unwrap().clone();
    assert_eq!(first_lookups.len(), EXTRA);
    assert_eq!(
        first_lookups
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        EXTRA
    );
    let requests_after_first = server.received_requests().await.unwrap();
    assert!(
        first.is_none(),
        "refused current decisions must not download"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        baseline_gets
    );
    let current_cursor: (String, String) = {
        let conn = db.acquire_lock("read refused current cursor").unwrap();
        let cursor: String = conn
            .query_row(
                "SELECT replay_after FROM provider_active_generations WHERE id=?1",
                [&peer_root.id],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&cursor).unwrap()
    };
    assert_eq!(
        current_cursor,
        (pass_key.clone(), "asset-extra-63".to_owned())
    );
    let historical = crate::download::replay_due_admitted_selection(
        &client,
        passes,
        Some(&resolved_unfiled_only()),
        None,
        owner_config.clone(),
        controls,
        CancellationToken::new(),
    )
    .await
    .expect("second fairness wake must complete");
    let second_lookups = lookups.lock().unwrap().clone();
    let second_lookups = &second_lookups[first_lookups.len()..];
    assert_eq!(second_lookups.len(), 64);
    assert_eq!(
        second_lookups
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        64
    );
    assert!(second_lookups.iter().any(|name| name == "asset-good"));
    assert!(second_lookups.iter().any(|name| name == "asset-wrong"));
    assert!(!second_lookups.iter().any(|name| name == "asset-future"));
    assert!(
        historical.is_some(),
        "still-due current refusals must not starve historical work"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        baseline_gets
    );
    assert!(historical.unwrap().stats.downloaded > 0);
    let requests_after_second = server.received_requests().await.unwrap();
    assert_eq!(requests_after_first.len(), baseline_gets);
    assert_eq!(
        requests_after_second
            .iter()
            .filter(|r| r.url.path() == "/good")
            .count(),
        1
    );
    assert_eq!(requests_after_second.len(), baseline_gets + 1);
    assert!(root.join("good.JPG").exists());
    let current_cursor: (String, String) = {
        let conn = db.acquire_lock("read bounded second cursor").unwrap();
        let cursor: String = conn
            .query_row(
                "SELECT replay_after FROM provider_active_generations WHERE id=?1",
                [&peer_root.id],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&cursor).unwrap()
    };
    assert_eq!(
        current_cursor,
        (pass_key.clone(), "asset-extra-61".to_owned())
    );
    let before_quiet = requests_after_second.len();
    for _ in 0..2 {
        let quiet = crate::download::replay_due_admitted_selection(
            &client,
            passes,
            Some(&resolved_unfiled_only()),
            None,
            owner_config.clone(),
            controls,
            CancellationToken::new(),
        )
        .await
        .expect("quiet wake must complete");
        assert!(quiet.is_none());
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        before_quiet,
        "settled source must stay quiet without GETs"
    );
    let restored = stock36_library_state(
        db.clone(),
        ZONE,
        restored_inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    for _ in 0..4 {
        crate::download::replay_due_admitted_selection(
            &client,
            restored.plan.passes.as_slice(),
            Some(&resolved_unfiled_only()),
            None,
            owner_config.clone(),
            controls,
            CancellationToken::new(),
        )
        .await
        .expect("restored current source must retry");
        let requests = server.received_requests().await.unwrap();
        if requests
            .iter()
            .filter(|r| r.url.path().starts_with("/extra-"))
            .count()
            == EXTRA
        {
            break;
        }
    }
    let requests = server.received_requests().await.unwrap();
    for index in 0..EXTRA {
        let path = format!("/extra-{index:02}");
        assert_eq!(
            requests.iter().filter(|r| r.url.path() == path).count(),
            1,
            "each current peer gets exactly one GET"
        );
        assert!(root.join(format!("extra-{index:02}.JPG")).exists());
    }
    let settled_gets = requests.len();
    for _ in 0..2 {
        assert!(
            crate::download::replay_due_admitted_selection(
                &client,
                restored.plan.passes.as_slice(),
                Some(&resolved_unfiled_only()),
                None,
                owner_config.clone(),
                controls,
                CancellationToken::new(),
            )
            .await
            .expect("settled wake must complete")
            .is_none()
        );
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        settled_gets
    );
    assert_eq!(
        generation_count(&db),
        generations_before,
        "retry must not readmit roots"
    );
    assert_eq!(epoch_state(&db, ZONE), epoch_before);
    assert_eq!(token(&db, ZONE), token_before);
    assert_eq!(decision_state(&db, "asset-future"), future_before);
    assert_eq!(decision_state(&db, "asset-wrong"), wrong_before);
    assert_eq!(
        requests.iter().filter(|r| r.url.path() == "/good").count(),
        1
    );
    eprintln!(
        "RETRY_FAIRNESS first_examined=64 second_examined=64 first_ack=extra-63 second_ack=extra-61 historical_gets=1 current_gets=64 future_gets=0 wrong_gets=0 quiet_wakes=2"
    );
    assert!(!root.join("wrong.JPG").exists());
    assert!(!root.join("future.JPG").exists());
    assert_eq!(full_queries.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn actual_pipeline_late_motion_failure_backs_off_failed_rendition_only() {
    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_run_cycle_download_config_builder,
        make_shared_session_for_run_cycle, seed_existing_upgrade_db,
    };
    use crate::download;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/still"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/motion"))
        .respond_with(wiremock::ResponseTemplate::new(410))
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/prepared"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let mut inventory = media_pair(ZONE, "live", "live.JPG", &format!("{}/still", server.uri()));
    inventory[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
        "downloadURL": format!("{}/motion", server.uri()), "size": MEDIA.len(), "fileChecksum": media_checksum()
    }});
    inventory[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let seed = media_pair(ZONE, "seed", "seed.JPG", &format!("{}/still", server.uri()));
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        seed,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let builder = |mode, excludes, groups, library| {
        let mut config = (*base(mode, excludes, groups, library)).clone();
        config.concurrent_downloads = 1;
        config.retry.max_retries = 0;
        Arc::new(config)
    };
    crate::sync_cycle::run_cycle(
        &[&selected],
        &config,
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .expect("seed cycle creates an authenticated current root");

    use crate::state::VersionSizeKey;
    use crate::state::db::provider_generations::{
        ActiveDecision, GenerationSpec, MAX_GENERATION_BYTES,
    };
    use crate::state::db::provider_selection::{
        SelectionDecision, SelectionDestination, SelectionOutcome, SelectionPath,
    };
    let (spec, pass_key): (GenerationSpec, String) = {
        let conn = db
            .acquire_lock("read actual current selection policy")
            .unwrap();
        let (bytes, key): (Vec<u8>, String) = conn.query_row(
            "SELECT g.specification,d.pass_key FROM provider_active_generations g JOIN provider_active_decisions d ON d.generation=g.id WHERE d.child='asset-seed'", [],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        (serde_json::from_slice(&bytes).unwrap(), key)
    };
    let generation = db
        .begin_selection_generation(baseline_owner(), spec.clone(), MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let body = serde_json::to_vec(&json!({"records":inventory})).unwrap();
    let photo = crate::icloud::photos::current_asset(
        &body,
        "asset-live",
        "live",
        &json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"}),
    )
    .unwrap();
    let mut destinations = Vec::new();
    let mut records = Vec::new();
    for (version, filename) in [
        (VersionSizeKey::Original, "live.JPG"),
        (VersionSizeKey::LiveOriginal, "live.MOV"),
    ] {
        let facts = &photo
            .versions()
            .iter()
            .find(|(key, _)| VersionSizeKey::from(*key) == version)
            .unwrap()
            .1;
        let metadata = photo.metadata_arc(version);
        destinations.push(SelectionDestination {
            version_size: version.as_str().into(),
            path: SelectionPath::from_path(&root.join(filename)),
            checksum: facts.checksum.to_string(),
            size: facts.size,
            metadata_hash: metadata.metadata_hash.clone().unwrap(),
        });
        records.push(
            crate::test_helpers::TestAssetRecord::new("asset-live")
                .library(ZONE)
                .version_size(version)
                .filename(filename)
                .checksum(&facts.checksum)
                .size(facts.size)
                .created_at(photo.created())
                .added_at(photo.added_date())
                .metadata((*metadata).clone())
                .build(),
        );
    }
    let manifest = ActiveDecision {
        state_id: "asset-live".into(),
        sources: vec![],
        decision: SelectionDecision {
            pass_key: pass_key.clone(),
            child: "asset-live".into(),
            master: Some("live".into()),
            confirmation: Some(body.clone()),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations,
        },
    };
    db.project_selection_decision_with_proof(
        baseline_owner(),
        generation.id.clone(),
        manifest.clone(),
        records.clone(),
        None,
        MAX_GENERATION_BYTES,
    )
    .await
    .unwrap();
    let before_identity = db
        .selection_retry_identity(
            baseline_owner(),
            generation.id.clone(),
            pass_key.clone(),
            "asset-live".into(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before_identity.destinations.len(), 2);
    let motion_destination = before_identity
        .destinations
        .iter()
        .find(|d| d.version_size == "live_original")
        .unwrap();
    assert_eq!(motion_destination.path.to_path(), root.join("live.MOV"));
    assert_eq!(motion_destination.checksum, media_checksum());
    assert_eq!(motion_destination.size, MEDIA.len() as u64);
    let owner_config = builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from(ZONE),
    );
    let prepared_pair = media_pair(
        ZONE,
        "prepared",
        "prepared.JPG",
        &format!("{}/prepared", server.uri()),
    );
    let prepared_path = root.join("prepared.JPG");
    admit_due_media(
        &db,
        &generation.id,
        &pass_key,
        ZONE,
        &prepared_pair,
        &prepared_path,
        0,
    )
    .await;
    use crate::state::db::{PreparedPublication, TempFileOwnershipStore};
    use sha2::{Digest, Sha256};
    let prepared_temp = download::test_temp_download_path(
        &prepared_path,
        &media_checksum(),
        &owner_config.temp_suffix,
    )
    .unwrap();
    std::fs::write(&prepared_temp, MEDIA).unwrap();
    db.claim_temp_file(&prepared_temp).await.unwrap();
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
    let proof = PreparedPublication {
        binding: None,
        library: ZONE.into(),
        id: "asset-prepared".into(),
        version: "original".into(),
        provider_checksum: media_checksum(),
        provider_size: MEDIA.len() as u64,
        temporary: SelectionPath::from_path(&prepared_temp),
        destination: SelectionPath::from_path(&prepared_path),
        local_checksum: digest.clone(),
        source_checksum: digest,
    };
    db.prepare_temp_publication(&prepared_temp, &proof)
        .await
        .unwrap();
    assert!(
        db.claim_temp_file(&prepared_temp).await.is_err(),
        "the real prepared journal must fence its staging owner"
    );
    let mut current_inventory =
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["records"]
            .as_array()
            .unwrap()
            .clone();
    current_inventory.extend(prepared_pair);
    let lookups = Arc::new(std::sync::Mutex::new(Vec::new()));
    let session = CountingRetrySession {
        inner: super::ResetCycleSession {
            zone: ZONE,
            inventory: Arc::new(current_inventory),
            full_queries: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            changes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail_full: false,
            auth_expired: false,
            boundary: "inventory-last-page",
        },
        lookups: Arc::clone(&lookups),
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let passes = [crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let before_requests = server.received_requests().await.unwrap().len();
    let result = crate::download::replay_due_admitted_selection(
        &reqwest::Client::new(),
        &passes,
        Some(&resolved_unfiled_only()),
        None,
        owner_config.clone(),
        download::DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("actual retry must attempt both renditions");
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(
        result.stats.failed, 2,
        "motion transfer and prepared staging both reach their actual failure paths"
    );

    let requests = server.received_requests().await.unwrap();
    let paths: Vec<_> = requests[before_requests..]
        .iter()
        .map(|r| r.url.path())
        .collect();
    assert_eq!(paths.iter().filter(|path| **path == "/still").count(), 1);
    assert_eq!(paths.iter().filter(|path| **path == "/motion").count(), 1);
    assert_eq!(paths.iter().filter(|path| **path == "/prepared").count(), 0);
    assert!(
        lookups
            .lock()
            .unwrap()
            .iter()
            .any(|name| name == "asset-prepared"),
        "the prepared owner must reach actual retry confirmation"
    );
    assert_eq!(
        paths.first(),
        Some(&"/still"),
        "the successful rendition must dispatch first"
    );
    assert_eq!(std::fs::read(&prepared_temp).unwrap(), MEDIA);
    assert!(!prepared_path.exists());
    assert_eq!(
        db.get_prepared_publications(ZONE, "asset-prepared", VersionSizeKey::Original)
            .await
            .unwrap(),
        vec![proof.clone()]
    );
    {
        let conn = db
            .acquire_lock("inspect exact live rendition backoff")
            .unwrap();
        let (reason, attempts, deadline): (String, i64, i64) = conn.query_row(
        "SELECT reason, attempts, next_retry_at FROM provider_active_decisions WHERE child='asset-live'", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
        let destinations: Vec<(String, bool)> = conn.prepare(
        "SELECT version_size, verified_media FROM provider_active_destinations WHERE child='asset-live' ORDER BY version_size"
    ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            destinations,
            vec![("live_original".into(), false), ("original".into(), true)]
        );
        let assets: Vec<(String, String)> = conn.prepare(
        "SELECT version_size,status FROM assets WHERE library=?1 AND id='asset-live' ORDER BY version_size"
    ).unwrap().query_map([ZONE], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            assets,
            vec![
                ("live_original".into(), "failed".into()),
                ("original".into(), "downloaded".into())
            ]
        );
        assert_eq!(
            reason, "download_failed",
            "the actual late motion failure must retain its own capture"
        );
        assert_eq!(attempts, 1);
        assert!(deadline > chrono::Utc::now().timestamp());
    }
    assert_eq!(std::fs::read(root.join("live.JPG")).unwrap(), MEDIA);
    assert_eq!(
        decision_state(&db, "asset-prepared"),
        (String::new(), 0, 0, 0)
    );
    let after_identity = db
        .selection_retry_identity(
            baseline_owner(),
            generation.id.clone(),
            pass_key.clone(),
            "asset-live".into(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_identity.generation, before_identity.generation);
    assert_eq!(
        after_identity.specification_hash,
        before_identity.specification_hash
    );
    assert_eq!(after_identity.manifest_hash, before_identity.manifest_hash);
    assert_ne!(
        after_identity.admission_hash,
        before_identity.admission_hash
    );
    assert_eq!(after_identity.destinations, before_identity.destinations);
    let before_quiet = requests.len();
    crate::download::replay_due_admitted_selection(
        &reqwest::Client::new(),
        &passes,
        Some(&resolved_unfiled_only()),
        None,
        owner_config,
        download::DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        before_quiet,
        "no sibling is fetched after completion or before the decision deadline"
    );
    assert_eq!(
        db.get_prepared_publications(ZONE, "asset-prepared", VersionSizeKey::Original)
            .await
            .unwrap(),
        vec![proof]
    );
    let newer = db
        .begin_selection_generation(baseline_owner(), spec, MAX_GENERATION_BYTES)
        .await
        .unwrap();
    let projection = db
        .project_selection_decision_with_proof(
            baseline_owner(),
            newer.id.clone(),
            manifest,
            records,
            None,
            MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    assert!(
        projection.admitted,
        "the same-resource successor must be an admitted owner"
    );
    let newer_before = db
        .selection_retry_identity(
            baseline_owner(),
            newer.id.clone(),
            pass_key.clone(),
            "asset-live".into(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newer_before.destinations, before_identity.destinations);
    assert_ne!(newer_before.generation, before_identity.generation);
    let mut stale = captured_from_identity(&before_identity);
    stale.destination = motion_destination.clone();
    assert!(
        !db.defer_selection_media_failure(baseline_owner(), stale)
            .await
            .unwrap()
    );
    let old_after_stale = db
        .selection_retry_identity(
            baseline_owner(),
            generation.id,
            pass_key.clone(),
            "asset-live".into(),
        )
        .await
        .unwrap()
        .unwrap();
    let newer_after = db
        .selection_retry_identity(baseline_owner(), newer.id, pass_key, "asset-live".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        old_after_stale.admission_hash,
        after_identity.admission_hash
    );
    assert_eq!(newer_after.admission_hash, newer_before.admission_hash);
    eprintln!(
        "RENDITION_RETRY still_gets=1 motion_gets=1 prepared_gets=0 still_verified=1 motion_failed=1 decision_attempts=1 prepared_journal_preserved=1 stale_cross_root_writes=0"
    );
}

#[tokio::test]
async fn actual_cleanup_recaptures_owner_after_confirmation_changes() {
    use super::{
        ZONE, baseline_owner, make_run_cycle_download_config_builder, seed_existing_upgrade_db,
    };
    use crate::download;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let server = wiremock::MockServer::start().await;
    let first_gets = Arc::new(AtomicUsize::new(0));
    let refreshed_gets = Arc::new(AtomicUsize::new(0));
    for (path, counter) in [("/first", &first_gets), ("/refreshed", &refreshed_gets)] {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(path))
            .respond_with(CountedGone(Arc::clone(counter)))
            .mount(&server)
            .await;
    }
    let initial_records = media_pair(
        ZONE,
        "refresh",
        "refresh.JPG",
        &format!("{}/first", server.uri()),
    );
    let mut refreshed_records = initial_records.clone();
    refreshed_records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/refreshed", server.uri()));
    let reset = |inventory| super::ResetCycleSession {
        zone: ZONE,
        inventory: Arc::new(inventory),
        full_queries: Arc::new(AtomicUsize::new(0)),
        changes: Arc::new(AtomicUsize::new(0)),
        fail_full: false,
        auth_expired: false,
        boundary: "inventory-last-page",
    };
    let mutation = Arc::new(std::sync::Mutex::new(None));
    let session = RefreshMutationSession {
        initial: reset(initial_records),
        refreshed: reset(refreshed_records),
        db: db.clone(),
        first_gets: Arc::clone(&first_gets),
        mutated: Arc::new(AtomicBool::new(false)),
        mutation: Arc::clone(&mutation),
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        baseline_owner(),
        "com",
    ));
    let passes = [crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let mut config = (*base(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from(ZONE),
    ))
    .clone();
    config.retry.max_retries = 0;
    config.concurrent_downloads = 1;
    download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(config),
        download::DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first_gets.load(Ordering::SeqCst), 1);
    assert_eq!(refreshed_gets.load(Ordering::SeqCst), 1);
    let (before, after) = mutation
        .lock()
        .unwrap()
        .clone()
        .expect("confirmation mutation occurs between transfers");
    assert_ne!(before, after);
    let (reason, attempts, deadline, verified) = decision_state(&db, "asset-refresh");
    assert_eq!(
        reason, "download_failed",
        "the final failed retry must use the refreshed pre-dispatch owner"
    );
    assert_eq!(
        attempts, 2,
        "the confirmation deferral and the later media failure each commit once"
    );
    assert!(deadline > chrono::Utc::now().timestamp());
    assert_eq!(verified, 0);
    eprintln!(
        "REFRESH_RETRY first_gets=1 refreshed_gets=1 confirmation_owner_updates=1 decision_attempts=2"
    );
}

#[tokio::test]
async fn current_due_retry_requires_resolved_unfiled_and_fresh_visibility() {
    use super::{
        MEDIA, ZONE, baseline_owner, make_run_cycle_config, make_run_cycle_download_config_builder,
        make_shared_session_for_run_cycle, seed_existing_upgrade_db,
    };
    use crate::download;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use tokio_util::sync::CancellationToken;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.path().join("state.db");
    seed_existing_upgrade_db(&db_path);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let seed = stock36_library_state(
        db.clone(),
        ZONE,
        media_pair(
            ZONE,
            "seed-vis",
            "seed.JPG",
            &format!("{}/seed", server.uri()),
        ),
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    run_stock36_cycle(&db, &[&seed], &root, &shared, &make_run_cycle_config())
        .await
        .unwrap();
    let (spec, key): (
        crate::state::db::provider_generations::GenerationSpec,
        String,
    ) = {
        let conn = db.acquire_lock("read current visibility policy").unwrap();
        let (bytes, key): (Vec<u8>, String) = conn.query_row(
            "SELECT g.specification,d.pass_key FROM provider_active_generations g JOIN provider_active_decisions d ON d.generation=g.id WHERE d.child='asset-seed-vis'", [],
            |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        (serde_json::from_slice(&bytes).unwrap(), key)
    };
    let generation = db
        .begin_selection_generation(
            baseline_owner(),
            spec,
            crate::state::db::provider_generations::MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    const REFUSED: [&str; 9] = [
        "hidden-vis",
        "null-vis",
        "unsupported-hidden",
        "deleted-master",
        "deleted-child",
        "null-master",
        "bad-deletion",
        "global-excluded",
        "resource-drift",
    ];
    let mut current = Vec::new();
    for id in std::iter::once("good-vis").chain(REFUSED) {
        let pair = media_pair(
            ZONE,
            id,
            &format!("{id}.JPG"),
            &format!("{}/{id}", server.uri()),
        );
        admit_due_media(
            &db,
            &generation.id,
            &key,
            ZONE,
            &pair,
            &root.join(format!("{id}.JPG")),
            0,
        )
        .await;
        let mut fresh = pair;
        match id {
            "hidden-vis" => fresh[1]["fields"]["isHidden"] = json!({"value":1}),
            "null-vis" => fresh[1]["fields"]["isHidden"] = json!({"value":null}),
            "unsupported-hidden" => fresh[1]["fields"]["isHidden"] = json!({"value":2}),
            "deleted-master" => fresh[0]["fields"]["isDeleted"] = json!({"value":1}),
            "deleted-child" => fresh[1]["fields"]["isDeleted"] = json!({"value":true}),
            "null-master" => fresh[0]["fields"]["isDeleted"] = json!({"value":null}),
            "bad-deletion" => fresh[1]["fields"]["isExpunged"] = json!({"value":null}),
            "resource-drift" => {
                fresh[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(MEDIA.len() + 1)
            }
            _ => {}
        }
        current.extend(fresh);
    }
    let selected = stock36_library_state(
        db.clone(),
        ZONE,
        current,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let selection = resolved_unfiled_only();
    let collection = crate::commands::build_collection_context(&selection, &[])
        .await
        .unwrap();
    let plan = crate::commands::resolve_passes_for_scope(
        &selected.library,
        &selection,
        crate::commands::PassScope {
            include_albums: false,
            include_smart_folders: false,
            include_unfiled: true,
        },
        &collection,
        &[],
    )
    .await
    .unwrap();
    assert_eq!(plan.passes.len(), 1);
    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let mut config = (*base(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from(ZONE),
    ))
    .clone();
    config.exclude_asset_ids = Arc::new(["asset-global-excluded".to_owned()].into_iter().collect());
    let config = Arc::new(config);
    let controls = download::DownloadControls::download_hidden();
    let before_gets = server.received_requests().await.unwrap().len();
    let epoch_before = epoch_state(&db, ZONE);
    let token_before = token(&db, ZONE);
    let before_debt: Vec<_> = REFUSED
        .iter()
        .map(|id| decision_state(&db, &format!("asset-{id}")))
        .collect();
    assert!(
        download::replay_due_admitted_selection(
            &reqwest::Client::new(),
            &plan.passes,
            None,
            None,
            config.clone(),
            controls,
            CancellationToken::new()
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), before_gets);
    let retried = download::replay_due_admitted_selection(
        &reqwest::Client::new(),
        &plan.passes,
        Some(&selection),
        None,
        config,
        controls,
        CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("valid absent-flags peer must progress");
    assert_eq!(retried.stats.downloaded, 1);
    assert!(retried.sync_token.is_none());
    assert_eq!(epoch_state(&db, ZONE), epoch_before);
    assert_eq!(token(&db, ZONE), token_before);
    let requests = server.received_requests().await.unwrap();
    let new_paths: Vec<_> = requests[before_gets..]
        .iter()
        .map(|r| r.url.path())
        .collect();
    assert_eq!(new_paths, vec!["/good-vis"]);
    assert_eq!(std::fs::read(root.join("good-vis.JPG")).unwrap(), MEDIA);
    for (index, id) in REFUSED.iter().enumerate() {
        assert_eq!(
            decision_state(&db, &format!("asset-{id}")),
            before_debt[index]
        );
        assert!(!root.join(format!("{id}.JPG")).exists());
    }
    eprintln!(
        "CURRENT_VISIBILITY valid_get=1 hidden_null_deleted_badflag_global_resource_get=0 unresolved_plan_get=0"
    );
}
