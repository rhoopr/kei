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

#[cfg(test)]
async fn stock36_library_state(
    db: std::sync::Arc<SqliteStateDb>,
    inventory: Vec<serde_json::Value>,
    full_queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    changes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> crate::sync_cycle::LibraryState {
    use std::sync::Arc;

    use super::{
        ResetCycleSession, ZONE, baseline_owner, make_run_cycle_library_state_with_passes,
        sync_token_key, zone,
    };
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoLibrary;

    let session = ResetCycleSession {
        zone: ZONE,
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
        Arc::new(zone()),
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
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), passes);
    selected.library = library;
    selected
}

async fn run_stock36_cycle(
    db: &std::sync::Arc<SqliteStateDb>,
    selected: &crate::sync_cycle::LibraryState,
    root: &std::path::Path,
    shared: &crate::auth::SharedSession,
    config: &crate::config::Config,
) -> anyhow::Result<crate::sync_cycle::CycleResult> {
    use super::make_run_cycle_download_config_builder;
    use crate::download;
    use tokio_util::sync::CancellationToken;

    let builder = make_run_cycle_download_config_builder(root, db.clone());
    crate::sync_cycle::run_cycle(
        &[selected],
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

/// A real stock-36 existing upgrade has no current receipt, token, epoch
/// completion marker or admitted destination. Its ordinary first baseline
/// admits a destination whose HTTP transfer fails through the same-cycle
/// cleanup. The affected baseline and token must stay held while the due item
/// retries at the next configured wake without a new inventory.
#[tokio::test]
async fn stock36_first_baseline_failed_media_retries_at_next_configured_wake_without_inventory() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        FlakyBytes, MEDIA, ZONE, baseline_owner, full_album_page_with_download,
        make_run_cycle_config, make_shared_session_for_run_cycle, seed_existing_upgrade_db,
        sync_token_key,
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
        .and(path("/fail"))
        .respond_with(FlakyBytes {
            calls: Arc::clone(&calls),
            bytes: Arc::new(MEDIA.to_vec()),
            // Phase 1 and the same-cycle cleanup pass both fail; the third
            // request belongs to the next normal wake.
            fail_first: 2,
        })
        .mount(&server)
        .await;

    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MEDIA));
    let mut page = full_album_page_with_download(
        ZONE,
        "fail",
        "inventory-last-page",
        &format!("{}/fail", server.uri()),
        MEDIA.len() as u64,
        &checksum,
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] =
        serde_json::json!(base64::engine::general_purpose::STANDARD.encode("fail.JPG"));
    page["records"][0]["recordChangeTag"] = serde_json::json!("ct-master-fail");
    page["records"][1]["recordChangeTag"] = serde_json::json!("ct-asset-fail");
    let inventory = page["records"].as_array().unwrap().clone();

    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        inventory.clone(),
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;

    let first = run_stock36_cycle(&db, &selected, &root, &shared, &config)
        .await
        .expect("first ordinary baseline must return a partial result");

    let (first_epoch_state, first_deadline, first_reason, admitted, verified) = {
        let conn = db.acquire_lock("inspect first baseline failure").unwrap();
        let state: String = conn
            .query_row(
                "SELECT json_extract(value,'$.state') FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        let (deadline, reason, admitted, verified): (i64, String, i64, i64) = conn
            .query_row(
                "SELECT d.next_retry_at,d.reason,w.admitted,w.verified_media FROM provider_active_decisions d JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child WHERE d.child='asset-fail'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        (state, deadline, reason, admitted, verified)
    };
    let first_token = db.get_metadata(&sync_token_key(ZONE)).await.unwrap();
    eprintln!(
        "L7 first cycle: failed={} full_queries={} calls={} epoch_state={first_epoch_state} item_deadline={first_deadline} item_reason={first_reason} admitted={admitted} verified_media={verified} token={first_token:?}",
        first.failed_count,
        full_queries.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
    );
    assert!(first.failed_count > 0);
    assert_eq!(full_queries.load(Ordering::SeqCst), 1);
    assert_eq!(admitted, 1);
    assert_eq!(verified, 0);
    assert!(first_deadline > chrono::Utc::now().timestamp());

    // Reopen, then advance only the exact recorded item deadline through the
    // existing owner hook. This represents the configured item wait elapsing;
    // selection, admission, receipts and destinations stay untouched.
    drop(selected);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    db.make_selection_retry_due(baseline_owner(), "asset-fail".to_owned())
        .await
        .unwrap();

    let second = run_stock36_cycle(&db, &selected, &root, &shared, &config)
        .await
        .expect("next configured wake must retry retained media");

    let (second_epoch_state, admitted, verified, token) = {
        let conn = db.acquire_lock("inspect next wake retry").unwrap();
        let state: String = conn
            .query_row(
                "SELECT json_extract(value,'$.state') FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        let (admitted, verified): (i64, i64) = conn
            .query_row(
                "SELECT w.admitted,w.verified_media FROM provider_active_destinations w WHERE w.child='asset-fail'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(ZONE)],
                |row| row.get(0),
            )
            .ok();
        (state, admitted, verified, token)
    };
    eprintln!(
        "L7 next wake: failed={} full_queries={} calls={} epoch_state={second_epoch_state} admitted={admitted} verified_media={verified} token={token:?}",
        second.failed_count,
        full_queries.load(Ordering::SeqCst),
        calls.load(Ordering::SeqCst),
    );

    assert_ne!(
        first_epoch_state, "complete",
        "a failed first baseline must stay held after its own incomplete work"
    );
    assert!(
        first_token.is_none(),
        "a failed first baseline must not publish checkpoint authority"
    );
    assert_eq!(
        full_queries.load(Ordering::SeqCst),
        1,
        "retained recorded work must retry without a new inventory"
    );
    assert_eq!(admitted, 1);
    assert_eq!(verified, 1, "the due admitted item must be retried");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        token, None,
        "an independent retry must not grant checkpoint authority"
    );
    assert_ne!(
        second_epoch_state, "complete",
        "the affected baseline stays held after an independent retry"
    );
}

/// A phase-1 failure that the same-cycle cleanup resolves is not media failure
/// evidence. It must leave no item retry deadline or confirmation reason.
#[tokio::test]
async fn stock36_cleanup_success_leaves_no_item_retry_backoff() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    use super::{
        FlakyBytes, MEDIA, ZONE, baseline_owner, full_album_page_with_download,
        make_run_cycle_config, make_shared_session_for_run_cycle, seed_existing_upgrade_db,
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
            // Phase 1 fails; the same-cycle cleanup succeeds.
            fail_first: 1,
        })
        .mount(&server)
        .await;

    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MEDIA));
    let mut page = full_album_page_with_download(
        ZONE,
        "recovered",
        "inventory-last-page",
        &format!("{}/recovered", server.uri()),
        MEDIA.len() as u64,
        &checksum,
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] =
        serde_json::json!(base64::engine::general_purpose::STANDARD.encode("recovered.JPG"));
    page["records"][0]["recordChangeTag"] = serde_json::json!("ct-master-recovered");
    page["records"][1]["recordChangeTag"] = serde_json::json!("ct-asset-recovered");
    let inventory = page["records"].as_array().unwrap().clone();

    let full_queries = Arc::new(AtomicUsize::new(0));
    let changes = Arc::new(AtomicUsize::new(0));
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );
    let selected = stock36_library_state(
        db.clone(),
        inventory,
        Arc::clone(&full_queries),
        Arc::clone(&changes),
    )
    .await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.retry.max_retries = 0;

    let result = run_stock36_cycle(&db, &selected, &root, &shared, &config)
        .await
        .expect("cleaned-up first baseline must complete");

    let (verified, reason, attempts, deadline): (i64, String, i64, i64) = {
        let conn = db.acquire_lock("inspect cleaned-up baseline item").unwrap();
        conn.query_row(
            "SELECT w.verified_media,d.reason,d.attempts,d.next_retry_at FROM provider_active_decisions d JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child WHERE d.child='asset-recovered'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    };
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
}
