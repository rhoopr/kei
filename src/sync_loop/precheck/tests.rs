use std::sync::Arc;

use crate::sync_cycle::LibraryState;
use crate::sync_loop::precheck::{
    DB_SYNC_TOKEN_KEY, DbPrecheckScope, SCOPED_DB_SYNC_TOKEN_PROVIDER,
    SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION, WatchPrecheck, check_changes_database,
    include_pending_local_work,
};
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, MetadataSetFailure, SCOPED_DB_SYNC_TOKEN_FAILURE_KEY, make_library_state,
    make_run_cycle_library_state, make_state_db,
};
use crate::{config, download, state};

#[test]
fn watch_precheck_skip_all_blocks_every_zone_and_token() {
    let precheck = WatchPrecheck::SkipAll;

    assert!(precheck.changed_zones().is_none());
    assert!(precheck.db_sync_token_after_success().is_none());
    assert!(!precheck.should_sync_zone("PrimarySync"));
    assert!(!precheck.should_sync_zone("SharedSync-123"));
}

#[test]
fn watch_precheck_proceed_all_allows_every_zone_without_db_token() {
    let precheck = WatchPrecheck::proceed_all();

    assert!(precheck.changed_zones().is_none());
    assert!(precheck.db_sync_token_after_success().is_none());
    assert!(precheck.should_sync_zone("PrimarySync"));
    assert!(precheck.should_sync_zone("SharedSync-123"));
}

#[test]
fn watch_precheck_changed_zones_scopes_sync_and_carries_db_token() {
    let mut zones = rustc_hash::FxHashSet::default();
    zones.insert("PrimarySync".to_string());
    let precheck = WatchPrecheck::Proceed {
        changed_zones: Some(zones),
        db_sync_token_after_success: Some("db-token-after-cycle".to_string()),
    };

    assert_eq!(
        precheck.db_sync_token_after_success(),
        Some("db-token-after-cycle")
    );
    assert_eq!(
        precheck
            .changed_zones()
            .expect("changed zone filter should be present")
            .len(),
        1
    );
    assert!(precheck.should_sync_zone("PrimarySync"));
    assert!(!precheck.should_sync_zone("SharedSync-123"));
}

#[test]
fn pending_metadata_work_bypasses_skip_for_only_affected_zone() {
    let mut precheck = WatchPrecheck::SkipAll;
    let mut local_zones = rustc_hash::FxHashSet::default();
    local_zones.insert("SharedSync-123".to_string());

    precheck.include_local_work_zones(local_zones);

    assert!(!precheck.should_sync_zone("PrimarySync"));
    assert!(precheck.should_sync_zone("SharedSync-123"));
    assert!(precheck.db_sync_token_after_success().is_none());
}

#[test]
fn pending_metadata_work_joins_provider_changed_zones_without_losing_db_token() {
    let mut changed_zones = rustc_hash::FxHashSet::default();
    changed_zones.insert("PrimarySync".to_string());
    let mut precheck = WatchPrecheck::Proceed {
        changed_zones: Some(changed_zones),
        db_sync_token_after_success: Some("db-token-after-cycle".to_string()),
    };
    let mut local_zones = rustc_hash::FxHashSet::default();
    local_zones.insert("SharedSync-123".to_string());

    precheck.include_local_work_zones(local_zones);

    assert!(precheck.should_sync_zone("PrimarySync"));
    assert!(precheck.should_sync_zone("SharedSync-123"));
    assert_eq!(
        precheck.db_sync_token_after_success(),
        Some("db-token-after-cycle")
    );
}

async fn seed_watch_metadata_work(
    db: &state::SqliteStateDb,
    library: &str,
    asset_id: &str,
    capture_revision: i64,
) {
    let record = crate::test_helpers::TestAssetRecord::new(asset_id)
        .library(library)
        .filename(&format!("{asset_id}.jpg"))
        .build();
    db.upsert_seen(&record).await.expect("seed asset");
    db.mark_downloaded(
        library,
        asset_id,
        "original",
        std::path::Path::new("unused-watch-precheck-photo.jpg"),
        "local-checksum",
        None,
    )
    .await
    .expect("mark asset downloaded");
    db.set_metadata_capture_revision_for_test(library, asset_id, capture_revision);
}

#[tokio::test]
async fn unresolved_identity_bypasses_watch_no_change_shortcut() {
    let db = state::SqliteStateDb::open_in_memory().unwrap();
    let marker = state::unresolved_identity_key("PrimarySync");
    db.set_metadata(&marker, "1").await.unwrap();
    let library = make_run_cycle_library_state("PrimarySync", "sync_token", "zone_token");
    let mut precheck = WatchPrecheck::SkipAll;
    include_pending_local_work(
        &mut precheck,
        &db,
        &config::MetadataConfig::default(),
        &[library],
    )
    .await;
    assert!(precheck.should_sync_zone("PrimarySync"));
    assert!(!precheck.should_sync_zone("SharedSync-other"));
    assert!(precheck.db_sync_token_after_success().is_none());
}

#[tokio::test]
async fn disabled_metadata_writers_leave_rewrite_marker_out_of_watch_work() {
    let db = state::SqliteStateDb::open_in_memory().expect("state db");
    seed_watch_metadata_work(
        &db,
        "PrimarySync",
        "REWRITE_DISABLED",
        state::METADATA_CAPTURE_REVISION,
    )
    .await;
    db.record_metadata_write_failure("PrimarySync", "REWRITE_DISABLED", "original")
        .await
        .expect("seed rewrite marker");
    let library = make_run_cycle_library_state("PrimarySync", "sync_token", "zone_token");
    let mut precheck = WatchPrecheck::SkipAll;

    include_pending_local_work(
        &mut precheck,
        &db,
        &config::MetadataConfig::default(),
        &[library],
    )
    .await;

    assert!(!precheck.should_sync_zone("PrimarySync"));
    assert_eq!(
        db.get_pending_metadata_rewrites_page(None, 0, 10)
            .await
            .expect("read rewrite markers")
            .len(),
        1,
        "disabling writers must retain durable retry evidence"
    );
}

#[tokio::test]
async fn enabled_metadata_writer_forces_only_selected_rewrite_zone() {
    let db = state::SqliteStateDb::open_in_memory().expect("state db");
    for (library, asset_id) in [
        ("PrimarySync", "REWRITE_SELECTED"),
        ("SharedSync-OTHER", "REWRITE_UNSELECTED"),
    ] {
        seed_watch_metadata_work(&db, library, asset_id, state::METADATA_CAPTURE_REVISION).await;
        db.record_metadata_write_failure(library, asset_id, "original")
            .await
            .expect("seed rewrite marker");
    }
    let selected = make_run_cycle_library_state("PrimarySync", "sync_token", "zone_token");
    let metadata = config::MetadataConfig {
        set_exif_rating: true,
        ..config::MetadataConfig::default()
    };
    let mut precheck = WatchPrecheck::SkipAll;

    include_pending_local_work(&mut precheck, &db, &metadata, &[selected]).await;

    assert!(precheck.should_sync_zone("PrimarySync"));
    assert!(!precheck.should_sync_zone("SharedSync-OTHER"));
}

#[tokio::test]
async fn capture_revision_forces_watch_work_with_metadata_writers_disabled() {
    let db = state::SqliteStateDb::open_in_memory().expect("state db");
    seed_watch_metadata_work(
        &db,
        "PrimarySync",
        "CAPTURE_PENDING",
        state::METADATA_CAPTURE_REVISION - 1,
    )
    .await;
    let library = make_run_cycle_library_state("PrimarySync", "sync_token", "zone_token");
    let mut precheck = WatchPrecheck::SkipAll;

    include_pending_local_work(
        &mut precheck,
        &db,
        &config::MetadataConfig::default(),
        &[library],
    )
    .await;

    assert!(precheck.should_sync_zone("PrimarySync"));
}

fn assert_proceed_changed(precheck: &WatchPrecheck, expected_zone: &str, expected_db: &str) {
    let WatchPrecheck::Proceed {
        changed_zones: Some(zones),
        db_sync_token_after_success: Some(db_token),
    } = precheck
    else {
        panic!("expected changed-zone proceed, got {precheck:?}");
    };
    assert_eq!(zones.len(), 1);
    assert!(
        zones.contains(expected_zone),
        "missing zone {expected_zone}"
    );
    assert_eq!(db_token, expected_db);
}

fn test_precheck_scope_for_states(states: &[LibraryState], scope_id: &str) -> DbPrecheckScope {
    let mut zones: Vec<String> = states.iter().map(|s| s.zone_name.clone()).collect();
    zones.sort();
    let selected_zones_json = serde_json::to_string(&zones).expect("serialize zones");
    let scope_json = serde_json::to_string(&serde_json::json!({
        "test_scope": scope_id,
        "selected_zones": zones,
    }))
    .expect("serialize scope");
    DbPrecheckScope {
        provider: SCOPED_DB_SYNC_TOKEN_PROVIDER.to_string(),
        account: "test@example.com".to_string(),
        shape_version: SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION,
        scope_hash: scope_id.to_string(),
        selected_zones_json,
        scope_json,
    }
}

async fn seed_scoped_db_token(
    db: &dyn state::SyncTokenStore,
    scope: &DbPrecheckScope,
    token: &str,
) {
    db.upsert_scoped_db_sync_token(scope.to_state_row(token))
        .await
        .expect("seed scoped db token");
}

async fn read_scoped_db_token(
    db: &dyn state::SyncTokenStore,
    scope: &DbPrecheckScope,
) -> Option<state::ScopedDbSyncToken> {
    db.get_scoped_db_sync_token(
        &scope.provider,
        &scope.account,
        scope.shape_version,
        &scope.scope_hash,
    )
    .await
    .expect("read scoped db token")
}

async fn check_single_library_changes_database(
    db: Option<&dyn download::DownloadStore>,
    lib_state: &LibraryState,
    svc: &mut crate::icloud::photos::PhotosService,
    scope: &DbPrecheckScope,
) -> WatchPrecheck {
    check_changes_database(
        db.map(|db| db as &dyn state::SyncTokenStore),
        std::slice::from_ref(lib_state),
        svc,
        scope,
    )
    .await
}

/// `more_coming=true` with empty zones must NOT skip the cycle.
/// Production logic: `if zones.is_empty() && !more_coming { skip }`.
/// A regression that flipped the conjunction would silently skip every
/// page-bearing wakeup -- silent loss of pending changes.
#[tokio::test]
async fn check_changes_database_more_coming_does_not_skip() {
    use serde_json::json;
    let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
        "syncToken": "db-tok-2",
        "moreComing": true,
        "zones": []
    }));
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );

    let db: Arc<dyn download::DownloadStore> = make_state_db();
    let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
    let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-more");
    seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

    let precheck =
        check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
            .await;

    assert!(
        matches!(
            precheck,
            WatchPrecheck::Proceed {
                changed_zones: None,
                db_sync_token_after_success: Some(ref token)
            } if token == "db-tok-2"
        ),
        "more_coming=true must not skip the cycle (more pages pending)"
    );
    let stored = read_scoped_db_token(db.as_ref(), &scope)
        .await
        .expect("scoped token should remain present");
    assert_eq!(stored.token, "db-tok-prev");
}

/// Empty zones + `more_coming=false` still skip this watch cycle, but
/// must not advance the scoped DB token.
/// A suspicious empty page should self-heal on the next wakeup by
/// rechecking from the last persisted token.
#[tokio::test]
async fn check_changes_database_empty_zones_skip_without_advancing_scoped_db_token() {
    use serde_json::json;
    let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
        "syncToken": "db-tok-3",
        "moreComing": false,
        "zones": []
    }));
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );

    let db: Arc<dyn download::DownloadStore> = make_state_db();
    let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
    let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-empty");
    seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

    let precheck =
        check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
            .await;

    assert_eq!(
        precheck,
        WatchPrecheck::SkipAll,
        "empty zones + more_coming=false must skip the cycle"
    );
    let stored = read_scoped_db_token(db.as_ref(), &scope)
        .await
        .expect("scoped token should still be present");
    assert_eq!(stored.token, "db-tok-prev");
}

/// A non-empty zones list MUST NOT skip — even
/// when more_coming=false. This is the real-work path; pinning it
/// alongside the skip path catches a flipped branch in either
/// direction.
#[tokio::test]
async fn check_changes_database_zone_changes_present_does_not_skip() {
    use serde_json::json;
    let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
        "syncToken": "db-tok-4",
        "moreComing": false,
        "zones": [
            {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "ps-tok-new"}
        ]
    }));
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );

    let db: Arc<dyn download::DownloadStore> = make_state_db();
    let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
    let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-changed");
    seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

    let precheck =
        check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
            .await;
    assert_proceed_changed(&precheck, "PrimarySync", "db-tok-4");
    let stored = read_scoped_db_token(db.as_ref(), &scope)
        .await
        .expect("scoped token should remain present");
    assert_eq!(stored.token, "db-tok-prev");
}

#[tokio::test]
async fn check_changes_database_multi_library_runs_only_changed_selected_zone() {
    use serde_json::json;
    let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
        "syncToken": "db-tok-shared",
        "moreComing": false,
        "zones": [
            {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "shared-tok-new"}
        ]
    }));
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );

    let db: Arc<dyn download::DownloadStore> = make_state_db();

    let states = vec![
        make_library_state("PrimarySync", "sync_token:PrimarySync"),
        make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
    ];
    let scope = test_precheck_scope_for_states(&states, "scope-shared");
    seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

    let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc, &scope).await;
    assert_proceed_changed(&precheck, "SharedSync-ABCD", "db-tok-shared");
}

#[tokio::test]
async fn check_changes_database_unselected_zone_change_skips_selected_libraries() {
    use serde_json::json;
    let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
        "syncToken": "db-tok-unselected",
        "moreComing": false,
        "zones": [
            {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "shared-tok-new"}
        ]
    }));
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );

    let db: Arc<dyn download::DownloadStore> = make_state_db();
    let states = vec![make_library_state("PrimarySync", "sync_token:PrimarySync")];
    let scope = test_precheck_scope_for_states(&states, "scope-unselected");
    seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

    let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc, &scope).await;
    assert_eq!(precheck, WatchPrecheck::SkipAll);
    let stored = read_scoped_db_token(db.as_ref(), &scope)
        .await
        .expect("token persisted");
    assert_eq!(stored.token, "db-tok-unselected");
}

/// No stored scoped DB token must not skip, but can capture a
/// changes/database token before the cycle. The token is only persisted
/// after the cycle completes cleanly, so concurrent changes remain safe.
#[tokio::test]
async fn check_changes_database_no_stored_token_bootstraps_without_skipping() {
    use serde_json::json;
    let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
        "syncToken": "db-token-bootstrap",
        "moreComing": false,
        "zones": [
            {
                "zoneID": {"zoneName": "PrimarySync"},
                "syncToken": "zone-token-bootstrap"
            }
        ]
    }));
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );

    // Empty DB - no scoped database pre-check row set.
    let db: Arc<dyn download::DownloadStore> = make_state_db();
    let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
    let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-missing");

    let precheck =
        check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
            .await;
    assert!(
        matches!(
            precheck,
            WatchPrecheck::Proceed {
                changed_zones: None,
                db_sync_token_after_success: Some(ref token)
            } if token == "db-token-bootstrap"
        ),
        "bootstrap token must be deferred until the cycle succeeds"
    );
}

#[tokio::test]
async fn check_changes_database_scoped_token_read_failure_proceeds_without_precheck() {
    let session = crate::test_helpers::MockPhotosSession::new();
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );
    let inner = make_state_db();
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(inner, "simulated scoped-token read failure")
            .with_get_failure(MetadataSetFailure::Exact(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY)),
    );

    let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
    let scope =
        test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-read-failure");
    let precheck =
        check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
            .await;

    assert_eq!(
        precheck,
        WatchPrecheck::proceed_all(),
        "scoped token read failure should fall back to the safe full cycle path"
    );
}

#[tokio::test]
async fn check_changes_database_legacy_db_token_without_scoped_row_does_not_skip() {
    let session = crate::test_helpers::MockPhotosSession::new();
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );
    let db: Arc<dyn download::DownloadStore> = make_state_db();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed legacy zone token");
    db.set_metadata(DB_SYNC_TOKEN_KEY, "db-tok-prev")
        .await
        .expect("seed legacy db token");

    let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
    let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-new");
    let precheck =
        check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
            .await;

    assert_eq!(
        precheck,
        WatchPrecheck::proceed_all(),
        "legacy unscoped db tokens are not scoped proof"
    );
}

#[tokio::test]
async fn check_changes_database_scope_hash_mismatch_does_not_skip() {
    let session = crate::test_helpers::MockPhotosSession::new();
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );
    let db: Arc<dyn download::DownloadStore> = make_state_db();
    let states = vec![make_library_state("PrimarySync", "sync_token:PrimarySync")];
    let narrow_scope = test_precheck_scope_for_states(&states, "recent-500");
    let broad_scope = test_precheck_scope_for_states(&states, "recent-1000");
    seed_scoped_db_token(db.as_ref(), &narrow_scope, "db-tok-narrow").await;

    let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc, &broad_scope).await;

    assert_eq!(
        precheck,
        WatchPrecheck::proceed_all(),
        "Phase 1 must require exact scope hash match"
    );
}

#[tokio::test]
async fn check_changes_database_corrupt_stored_scope_json_does_not_skip() {
    let session = crate::test_helpers::MockPhotosSession::new();
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );
    let db: Arc<dyn download::DownloadStore> = make_state_db();
    let states = vec![make_library_state("PrimarySync", "sync_token:PrimarySync")];
    let scope = test_precheck_scope_for_states(&states, "corrupt-scope");
    db.upsert_scoped_db_sync_token(state::ScopedDbSyncToken {
        provider: scope.provider.clone(),
        account: scope.account.clone(),
        shape_version: scope.shape_version,
        scope_hash: scope.scope_hash.clone(),
        selected_zones_json: scope.selected_zones_json.clone(),
        scope_json: "{not valid json".to_string(),
        token: "db-tok-corrupt".to_string(),
    })
    .await
    .expect("seed corrupt scoped token row");

    let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc, &scope).await;

    assert_eq!(
        precheck,
        WatchPrecheck::proceed_all(),
        "corrupt stored scope JSON must fall back to enumeration"
    );
}

/// A scoped-token write failure on the
/// unselected-zone skip path must not break watch mode.
#[tokio::test]
async fn check_changes_database_unselected_zone_token_persist_failure_still_skips() {
    use serde_json::json;
    let inner = make_state_db();
    let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
    let scope =
        test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-write-fail");
    seed_scoped_db_token(inner.as_ref(), &scope, "db-tok-prev").await;
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        inner,
        MetadataSetFailure::Exact(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY),
        "simulated scoped db sync token write failure",
    ));

    let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
        "syncToken": "db-tok-bad-write",
        "moreComing": false,
        "zones": [
            {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "ss-tok-new"}
        ]
    }));
    let mut svc = crate::icloud::photos::PhotosService::for_testing(
        Box::new(session),
        std::collections::HashMap::new(),
    );

    let precheck =
        check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
            .await;
    assert_eq!(precheck, WatchPrecheck::SkipAll);
}
