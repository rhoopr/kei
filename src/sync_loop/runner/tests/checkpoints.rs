use crate::state::{DownloadStateStore, ReportStateStore};
use crate::state::{MembershipStore, SyncTokenStore};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{
    ENUM_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, determine_sync_mode, run_cycle,
    should_store_sync_token, should_store_sync_token_for_cycle,
};
use crate::sync_loop::test_support::{
    RunCycleDownloadConfigOptions, album_count_response, full_album_page,
    full_album_page_with_download, make_recording_run_cycle_download_config_builder,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle, make_state_db,
    media_without_photo_downloads, run_cycle_expected_date_dir,
};
use crate::{download, state};

#[derive(Clone)]
struct LegacyPendingDeleteSession {
    master_record_name: Arc<str>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for LegacyPendingDeleteSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/changes/zone?") {
            let body: serde_json::Value = serde_json::from_str(&body)?;
            let records = if body["zones"][0]["syncToken"] == "zone-token-before-legacy-cleanup" {
                vec![
                    serde_json::json!({"recordName":self.master_record_name.as_ref(), "recordType":"CPLMaster", "deleted":true}),
                ]
            } else {
                Vec::new()
            };
            return Ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-token-after-legacy-cleanup",
                    "moreComing": false,
                    "records": records
                }]
            }));
        }
        if url.contains("/records/lookup?") {
            anyhow::bail!("Historical pending state must not schedule a lookup");
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(0));
        }
        Ok(serde_json::json!({"records": [], "syncToken":"zone-token-after-legacy-cleanup"}))
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn run_cycle_recent_exact_inventory_stores_zone_token() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let owned_db =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = owned_db.clone();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let session = crate::test_helpers::DynamicRecentPhotosSession::new(40)
        .with_filename_prefix("cycle-recent")
        .with_token("zone-tok-recent");
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &owned_db,
        "PrimarySync",
        "private",
        Box::new(session),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run recent cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.assets_seen, 40);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token"),
        Some("zone-tok-recent".to_owned()),
        "an N+1 probe that proves exact EOF may persist the zone token"
    );
    let events = capture.events();
    assert!(
        !events
            .iter()
            .any(|event| { event.field("reason") == Some("recent_limited_full_enumeration") }),
        "an exact recent inventory must not report truncation: {events:?}"
    );
    assert!(
        !events.iter().any(|event| {
            event.level == tracing::Level::WARN
                && event.field("reason") == Some("recent_limited_full_enumeration")
        }),
        "recent-limited token suppression must not warn: {events:?}"
    );
}

#[tokio::test]
async fn run_cycle_true_token_unsafe_condition_still_warns() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let directory = tempfile::tempdir().unwrap();
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&directory.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let library = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "PrimarySync",
            "private",
            Box::new(crate::test_helpers::DynamicRecentPhotosSession::new(0).with_token("")),
        ),
    );
    let config = make_run_cycle_config();
    let builder = make_run_cycle_download_config_builder(directory.path(), db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result.failed_count, 1);
    assert_eq!(result.stats.failed, 0);
    assert!(!result.can_advance_database_checkpoint());
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some("icloud_blank_sync_token")
    );
    let events = capture.events();
    assert!(
        events.iter().any(|event| {
            event.level == tracing::Level::WARN
                && event.field("diagnostic") == Some("icloud_blank_sync_token")
                && event
                    .message()
                    .is_some_and(|message| message.contains("Provider checkpoint preserved"))
        }),
        "true token-unsafe sync-token suppression should still warn: {events:?}"
    );
}

#[tokio::test]
async fn contract_malformed_required_asset_fields_block_checkpoint() {
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "stale-enum-config")
        .await
        .expect("seed stale enum config hash");

    let mut page = full_album_page("PrimarySync", "", "zone-tok-new");
    page["records"][0]["recordName"] = serde_json::json!("");
    page["records"][1]["recordName"] = serde_json::json!("");
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(page),
        ),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run malformed identity cycle");

    assert!(result.failed_count > 0);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(result.stats.skipped.total(), 0);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "paired blank identities must replay from the prior checkpoint"
    );
}

#[tokio::test]
async fn watch_recent_exact_first_cycle_seeds_incremental_token() {
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let database = download_dir.path().join("state.db");
    let owned_db = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    let db: Arc<dyn download::DownloadStore> = owned_db.clone();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
        .with_filename_prefix("watch-recent")
        .with_token("zone-tok-watch");
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &owned_db,
        "PrimarySync",
        "private",
        Box::new(session.clone()),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run first watch-like recent cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token"),
        Some("zone-tok-watch".to_owned())
    );
    let next_mode = determine_sync_mode(
        1,
        Some(db.as_ref()),
        "sync_token:PrimarySync",
        "PrimarySync",
    )
    .await;
    assert!(
        matches!(next_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "zone-tok-watch"),
        "a later watch cycle should use the token from proved recent EOF"
    );
    let queries_after_first = session.offsets();
    for _ in 0..2 {
        let reopened = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        let db: Arc<dyn download::DownloadStore> = reopened.clone();
        let album = crate::sync_loop::test_support::make_owned_cycle_album(
            &reopened,
            "PrimarySync",
            "private",
            Box::new(session.clone()),
        );
        let state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let builder = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let quiet = run_cycle(
            &[&state],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(quiet.failed_count, 0);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(
            session.offsets(),
            queries_after_first,
            "quiet cycles must not enumerate again"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-tok-watch")
        );
    }
}

/// Recovery work is independent from source enumeration.
#[tokio::test]
async fn determine_sync_mode_with_pending_failure_keeps_stored_token() {
    let db = make_state_db();
    let sync_token_key = "sync_token:PrimarySync";
    db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("failed").build())
        .await
        .unwrap();
    db.mark_failed("PrimarySync", "failed", "original", "transfer failed")
        .await
        .unwrap();
    db.set_metadata(sync_token_key, "stored-token-abc")
        .await
        .expect("set token");

    let mode = determine_sync_mode(1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;

    assert!(
        matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
        "pending failure should keep source tracking incremental, got {mode:?}"
    );
}

/// Mode selection must neither consume nor clear the stored provider token.
#[tokio::test]
async fn determine_sync_mode_does_not_consume_or_clear_stored_token() {
    let db = make_state_db();
    let sync_token_key = "sync_token:PrimarySync";
    db.set_metadata(sync_token_key, "stored-token-abc")
        .await
        .expect("set token");

    let retry_mode = determine_sync_mode(1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
    assert!(
        matches!(retry_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
        "mode selection should use the stored token, got {retry_mode:?}"
    );

    let normal_mode =
        determine_sync_mode(1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
    assert!(
        matches!(normal_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
        "repeated mode selection should still use the stored token, got {normal_mode:?}"
    );
}

#[tokio::test]
async fn determine_sync_mode_empty_stored_token_falls_back_to_full() {
    let db = make_state_db();
    let sync_token_key = "sync_token:PrimarySync";
    // Empty-string token is the malformed case — should be ignored.
    db.set_metadata(sync_token_key, "")
        .await
        .expect("set empty token");

    let mode = determine_sync_mode(1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;

    assert!(
        matches!(mode, download::SyncMode::Full),
        "empty stored token must yield Full, got {mode:?}"
    );

    // And a present, non-empty token must trigger Incremental — pin the
    // happy path here too so the empty-string check can't be dropped
    // without breaking both assertions.
    db.set_metadata(sync_token_key, "real-token")
        .await
        .expect("set real token");
    let mode = determine_sync_mode(1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
    assert!(
        matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "real-token"),
        "non-empty token must yield Incremental with that token, got {mode:?}"
    );
}

/// When the state DB read fails, fall back to Full rather than
/// propagating. The watch loop must keep going even if sqlite hiccups —
/// silently biasing toward Incremental on errors would mask data loss.
///
/// The inline `FailingDb` implements only `SyncTokenStore`, so any future
/// reroute inside `determine_sync_mode` has to either stay inside the
/// token role or fail to compile.
#[tokio::test]
async fn determine_sync_mode_state_db_error_falls_back_to_full() {
    // Minimal failing impl: only `get_metadata` is reachable from
    // `determine_sync_mode`; the narrow role trait keeps any silent
    // reroute from compiling against this stub.
    struct FailingDb;

    #[async_trait::async_trait]
    impl state::SyncTokenStore for FailingDb {
        async fn get_metadata(&self, _: &str) -> Result<Option<String>, state::error::StateError> {
            Err(state::error::StateError::LockPoisoned("simulated".into()))
        }

        async fn set_metadata(&self, _: &str, _: &str) -> Result<(), state::error::StateError> {
            unimplemented!()
        }

        async fn delete_metadata_by_prefix(
            &self,
            _: &str,
        ) -> Result<u64, state::error::StateError> {
            unimplemented!()
        }

        async fn begin_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
            unimplemented!()
        }

        async fn end_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
            unimplemented!()
        }

        async fn list_interrupted_enumerations(
            &self,
        ) -> Result<Vec<String>, state::error::StateError> {
            unimplemented!()
        }
    }

    let db = FailingDb;

    let mode = determine_sync_mode(1, Some(&db), "sync_token:PrimarySync", "PrimarySync").await;

    assert!(
        matches!(mode, download::SyncMode::Full),
        "DB read error must fall back to Full, got {mode:?}"
    );
}

/// Sanity: `state_db = None` (e.g. legacy run with no state path) must
/// still produce Full. The match-arm exists in production today; pin
/// it so a future refactor that drops the `else` branch tells us.
#[tokio::test]
async fn determine_sync_mode_no_state_db_returns_full() {
    let mode = determine_sync_mode::<state::SqliteStateDb>(
        1,
        None,
        "sync_token:PrimarySync",
        "PrimarySync",
    )
    .await;

    assert!(
        matches!(mode, download::SyncMode::Full),
        "no state DB must yield Full, got {mode:?}"
    );
}

// `should_store_sync_token` is the single decision gate protecting the
// sync-token from being advanced after a partial sync or a dry run. Both
// situations would lose change events on the next incremental cycle
// ("user data is sacred"). The matrix below pins every (outcome, dry_run)
// combination so a future refactor can't relax the contract without a
// failing test.

/// A partial download failure MUST NOT advance the stored sync
/// token. Otherwise the next incremental sync would skip past the
/// failed assets' change events and never retry them.
#[test]
fn contract_sync_token_advance_requires_clean_cycle_blocks_partial_failure() {
    let outcome = download::DownloadOutcome::PartialFailure { failed_count: 3 };
    assert!(
        !should_store_sync_token(&outcome, false),
        "PartialFailure must NOT advance the sync token, even outside dry-run"
    );
    // dry_run=true cannot rescue a partial failure either.
    assert!(
        !should_store_sync_token(&outcome, true),
        "PartialFailure + dry_run must still NOT advance the sync token"
    );
}

/// `SessionExpired` is also a non-success outcome and
/// MUST NOT advance the token. The cycle aborts mid-stream; the captured
/// token may only reflect a subset of the work.
#[test]
fn sync_loop_session_expired_does_not_advance_sync_token() {
    let outcome = download::DownloadOutcome::SessionExpired {
        auth_error_count: 5,
    };
    assert!(!should_store_sync_token(&outcome, false));
    assert!(!should_store_sync_token(&outcome, true));
}

/// In `--dry-run`, even a fully-successful pass MUST NOT advance
/// the token. Dry-run promises no DB writes that affect the next real
/// sync; advancing the token would silently break the next incremental.
#[test]
fn sync_loop_dry_run_does_not_advance_sync_token() {
    let outcome = download::DownloadOutcome::Success;
    assert!(
        !should_store_sync_token(&outcome, true),
        "dry_run must NOT advance the sync token even on Success"
    );
}

/// Positive control: only the (Success, dry_run=false) combination
/// advances the token. Pinning this branch prevents a future refactor
/// from accidentally inverting the predicate.
#[test]
fn sync_loop_full_success_outside_dry_run_advances_sync_token() {
    let outcome = download::DownloadOutcome::Success;
    assert!(
        should_store_sync_token(&outcome, false),
        "(Success, dry_run=false) is the ONLY combination that should advance the token"
    );
}

/// A library that consumed a stale plan from a prior failed
/// `resolve_passes` MUST NOT advance its sync token even when its
/// outcome is `Success`. A reused plan can route assets to the wrong
/// pass; advancing the affected zone token would skip the change events
/// that would surface the corrected membership on the next cycle.
#[test]
fn sync_loop_stale_plan_blocks_sync_token_advance_even_on_success() {
    let outcome = download::DownloadOutcome::Success;
    // Baseline: without a stale plan, Success advances the token.
    assert!(should_store_sync_token_for_cycle(&outcome, false, false));
    // With a stale plan: even Success must NOT advance the token.
    assert!(
        !should_store_sync_token_for_cycle(&outcome, false, true),
        "same-library stale-plan flag must veto token advancement on Success"
    );
}

/// Stale-plan companion: dry_run and PartialFailure already block; pinning
/// the matrix so a future refactor can't silently change the AND/OR
/// shape of the gate.
#[test]
fn sync_loop_stale_plan_combines_with_existing_gates() {
    let success = download::DownloadOutcome::Success;
    let partial = download::DownloadOutcome::PartialFailure { failed_count: 1 };

    // PartialFailure: blocked regardless of stale-plan flag.
    assert!(!should_store_sync_token_for_cycle(&partial, false, false));
    assert!(!should_store_sync_token_for_cycle(&partial, false, true));

    // Dry-run: blocked regardless of stale-plan flag.
    assert!(!should_store_sync_token_for_cycle(&success, true, false));
    assert!(!should_store_sync_token_for_cycle(&success, true, true));

    // Only (Success, dry_run=false, library_stale=false) advances.
    assert!(should_store_sync_token_for_cycle(&success, false, false));
    assert!(!should_store_sync_token_for_cycle(&success, false, true));
}

#[tokio::test]
async fn run_cycle_clean_zone_advances_despite_other_stale_plan() {
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    db.set_metadata("sync_token:PrimarySync", "primary-prev")
        .await
        .expect("seed primary token");
    db.set_metadata("sync_token:SharedSync-TEST", "shared-prev")
        .await
        .expect("seed shared token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let clean_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "PrimarySync",
            "private",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0).with_token("primary-new"),
            ),
        ),
    );
    let mut stale_state = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "SharedSync-TEST",
            "shared",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0)
                    .with_zone("SharedSync-TEST")
                    .with_token("shared-new"),
            ),
        ),
    );
    stale_state.plan_is_stale = true;
    let states = vec![&clean_state, &stale_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert!(
        !result.db_sync_token_advance_safe,
        "database precheck token must wait for every selected zone to be clean"
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read primary token")
            .as_deref(),
        Some("primary-new"),
        "unaffected clean zone should advance its own token"
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .expect("read shared token")
            .as_deref(),
        Some("shared-prev"),
        "stale zone must keep its old token"
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn primary_cycle_finishes_both_libraries_before_any_local_backfill() {
    use crate::sync_loop::test_support::{
        MutableCaptureSession, make_owned_cycle_album, open_owned_cycle_db,
    };
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let inner = open_owned_cycle_db(&dir.path().join("state.db")).await;
    let store: Arc<dyn download::DownloadStore> = inner.clone();
    let server = wiremock::MockServer::start().await;
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .expect(2)
        .mount(&server)
        .await;
    let mut states = Vec::new();
    for (zone, database) in [("PrimarySync", "private"), ("SharedSync-TEST", "shared")] {
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
        let mut page = full_album_page_with_download(
            zone,
            zone,
            "current",
            &server.uri(),
            bytes.len() as u64,
            &checksum,
        );
        page["records"][0]["fields"]["filenameEnc"]["value"] =
            serde_json::json!(format!("{zone}.jpg"));
        page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
            serde_json::json!("_defaultOwner");
        let album = make_owned_cycle_album(
            &inner,
            zone,
            database,
            Box::new(MutableCaptureSession {
                zone,
                token: "current",
                cancel_after_lookup: None,
                records: Arc::new(std::sync::Mutex::new(
                    page["records"].as_array().unwrap().clone(),
                )),
                repair_requests: Default::default(),
            }),
        );
        states.push(make_run_cycle_library_state_with_album(
            zone,
            &format!("sync_token:{zone}"),
            album,
        ));
    }
    let media_dir = tempfile::tempdir().unwrap();
    let base = make_run_cycle_download_config_builder(media_dir.path(), store.clone());
    let builder = |mode, excluded, groups, library| {
        let mut config = base(mode, excluded, groups, library);
        let c = Arc::make_mut(&mut config);
        c.post_sync_metadata = true;
        c.metadata.xmp_sidecar = true;
        config
    };
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let cfg = make_run_cycle_config();
    let controls = download::DownloadControls::download_hidden();
    let cancel = CancellationToken::new();
    let result = run_cycle(
        &states.iter().collect::<Vec<_>>(),
        &cfg,
        Some(store.as_ref()),
        &builder,
        controls,
        &session,
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(result.stats.downloaded, 2, "{:?}", result.stats);
    assert!(
        result.can_advance_database_checkpoint(),
        "{:?}",
        result.stats
    );
    let pending = || {
        inner.acquire_lock("retained metadata for both libraries").unwrap().query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1 AND verified_metadata=0",[],|r|r.get(0)).unwrap()
    };
    assert_eq!(pending(), 2);
    for library in &states {
        assert_eq!(
            store
                .get_metadata(&library.sync_token_key)
                .await
                .unwrap()
                .as_deref(),
            Some("current")
        );
    }
    for library in &states {
        let config = builder(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from(library.zone_name.as_str()),
        );
        let result = Box::pin(download::drain_local_backfill(
            &library.plan.passes,
            &config,
            &inner,
            controls,
            &cancel,
            None,
            &mut Default::default(),
        ))
        .await
        .unwrap();
        assert_eq!(result.failed, 0);
    }
    assert_eq!(pending(), 0);
}

#[tokio::test]
async fn run_cycle_stale_plan_blocks_database_precheck_token() {
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let mut lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "PrimarySync",
            "private",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0).with_token("zone-tok-new"),
            ),
        ),
    );
    lib_state.plan_is_stale = true;
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert!(
        !result.db_sync_token_advance_safe,
        "database precheck token must wait until stale plans stop suppressing zone tokens"
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "stale plan must leave the old zone token in place for replay"
    );
    assert!(!config.runtime.dry_run);
}

#[tokio::test]
async fn run_cycle_zone_token_write_failure_blocks_database_precheck_token() {
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let database = download_dir.path().join("state.db");
    let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    inner
        .acquire_lock("inject cursor write failure")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_zone_cursor BEFORE INSERT ON metadata
         WHEN NEW.key = 'sync_token:PrimarySync'
         BEGIN SELECT RAISE(ABORT, 'simulated sync-token write failure'); END;",
        )
        .unwrap();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "PrimarySync",
            "private",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0).with_token("zone-tok-new"),
            ),
        ),
    );
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 1);
    assert_eq!(result.stats.state_write_failures, 1);
    assert!(
        !result.db_sync_token_advance_safe,
        "database precheck token must not advance after a zone-token write failure"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "failed zone-token write must leave old token in place for replay"
    );
    drop(build_download_config);
    drop(states);
    drop(lib_state);
    drop(db);
    drop(inner);
    for replay in 0..2 {
        let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        if replay == 0 {
            assert_eq!(
                inner
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("zone-tok-prev")
            );
            inner
                .acquire_lock("remove cursor write failure")
                .unwrap()
                .execute_batch("DROP TRIGGER fail_zone_cursor")
                .unwrap();
        }
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let library = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            crate::sync_loop::test_support::make_owned_cycle_album(
                &inner,
                "PrimarySync",
                "private",
                Box::new(
                    crate::test_helpers::DynamicRecentPhotosSession::new(0)
                        .with_token("zone-tok-new"),
                ),
            ),
        );
        let builder = make_run_cycle_download_config_builder(download_dir.path(), db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.downloaded, 0);
        assert!(result.db_sync_token_advance_safe);
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-tok-new")
        );
    }
}

fn checkpoint_download_session(
    master: &str,
    token: &'static str,
    url: &str,
    size: u64,
    checksum: &str,
) -> crate::sync_loop::test_support::MutableCaptureSession {
    let mut page = full_album_page_with_download("PrimarySync", master, token, url, size, checksum);
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        serde_json::json!("_defaultOwner");
    crate::sync_loop::test_support::MutableCaptureSession {
        zone: "PrimarySync",
        token,
        cancel_after_lookup: None,
        records: Arc::new(std::sync::Mutex::new(
            page["records"].as_array().unwrap().clone(),
        )),
        repair_requests: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    }
}

#[tokio::test]
async fn run_cycle_published_file_state_write_failure_blocks_token() {
    // A published file with a failed state write is unsafe to skip on the
    // next incremental cycle, even though the media bytes are on disk.
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
            .await;
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    inner
        .acquire_lock("inject downloaded state failure")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_downloaded BEFORE UPDATE ON assets WHEN NEW.status = 'downloaded'
         BEGIN SELECT RAISE(ABORT, 'simulated mark_downloaded failure'); END;",
        )
        .unwrap();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let master_record_name = "master-state-write-failure";
    let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(checkpoint_download_session(
            master_record_name,
            "zone-tok-after-state-write-failure",
            &format!("{}/photo.jpg", server.uri()),
            body.len() as u64,
            &checksum,
        )),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    let final_dir = download_dir.path().join(run_cycle_expected_date_dir());
    let final_paths = std::fs::read_dir(&final_dir)
        .expect("final directory exists")
        .map(|entry| entry.expect("final file entry").path())
        .collect::<Vec<_>>();
    assert_eq!(
        final_paths.len(),
        1,
        "expected one final file: {final_paths:?}"
    );
    assert_eq!(
        std::fs::read(&final_paths[0]).expect("read local media file"),
        body,
        "media bytes must land before the state write fails"
    );
    assert_eq!(
        result.failed_count, 1,
        "state-write failure must make the cycle partial; stats: {:?}",
        result.stats
    );
    assert_eq!(result.stats.state_write_failures, 1);
    assert!(
        !result.db_sync_token_advance_safe,
        "database precheck token must not advance after a state-write failure"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "failed mark_downloaded must leave the old zone token in place for replay"
    );
    assert!(
        inner
            .get_downloaded_page(0, 10)
            .await
            .expect("downloaded rows")
            .is_empty(),
        "asset must not be marked fully downloaded when mark_downloaded fails"
    );
}

#[tokio::test]
async fn run_cycle_album_membership_failure_preserves_and_replays_checkpoint() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
            .await;
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let session = checkpoint_download_session(
        "master-PrimarySync",
        "zone-tok-new",
        &format!("{}/photo.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(session),
    );
    let lib_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            per_pass_paths: true,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    inner.fail_asset_album_writes_for_test();
    let failed = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run failed grouping cycle");

    assert_eq!(failed.failed_count, 2, "cycle stats: {:?}", failed.stats);
    assert_eq!(
        failed.stats.state_write_failures, 1,
        "cycle stats: {:?}",
        failed.stats
    );
    assert!(failed.stats.identity_incomplete);
    assert_eq!(failed.stats.failed, 0, "admission failed before transfer");
    assert_eq!(failed.stats.downloaded, 0);
    assert_eq!(failed.stats.bytes_downloaded, 0);
    assert!(inner.get_downloaded_page(0, 10).await.unwrap().is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
    let mut directories = vec![download_dir.path().to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            assert!(
                entry.file_type().unwrap().is_dir(),
                "grouping must become durable before any file is published: {:?}",
                entry.path()
            );
            directories.push(entry.path());
        }
    }
    assert!(!failed.db_sync_token_advance_safe);
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read preserved zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "a missing grouping relationship must preserve the replay checkpoint"
    );
    assert!(
        inner
            .get_all_asset_albums("PrimarySync")
            .await
            .expect("read missing grouping relationship")
            .is_empty()
    );

    inner.allow_asset_album_writes_for_test();
    let mut published_path = None;
    for cycle in 2..=3 {
        let recovered = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("grouping replay cycle {cycle} failed: {error}"));

        assert_eq!(recovered.failed_count, 0);
        assert_eq!(recovered.stats.state_write_failures, 0);
        assert!(!recovered.stats.identity_incomplete);
        assert!(recovered.can_advance_database_checkpoint(), "{recovered:?}");
        assert_eq!(
            recovered.stats.downloaded,
            usize::from(cycle == 2),
            "publish once after grouping recovery, then remain quiet"
        );
        let rows = inner.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_ref(), "asset-master-PrimarySync");
        let path = rows[0].local_path.as_ref().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), body);
        if let Some(previous) = &published_path {
            assert_eq!(
                path, previous,
                "quiet replay must retain the publication path"
            );
        }
        published_path = Some(path.clone());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(
            inner
                .get_all_asset_albums("PrimarySync")
                .await
                .expect("read recovered grouping relationship"),
            vec![(
                "asset-master-PrimarySync".to_string(),
                "TestAlbum".to_string()
            )],
            "cycle {cycle} must leave one idempotent relationship"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-tok-new")
        );
    }
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read recovered zone token")
            .as_deref(),
        Some("zone-tok-new"),
        "the checkpoint may advance after the grouping relationship is durable"
    );
    assert_eq!(
        std::fs::read(published_path.unwrap()).expect("read stable media"),
        body,
        "grouping replay must not change landed media"
    );
    server.verify().await;
}

#[tokio::test]
async fn run_cycle_durable_expired_url_failure_holds_zone_checkpoint_until_publication() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(2)
        .mount(&server)
        .await;

    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let database = state_dir.path().join("state.db");
    let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let body = b"\xff\xd8\xff\xe0\x00\x10JFIF";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let session = checkpoint_download_session(
        "master-expired",
        "zone-tok-next",
        &format!("{}/expired.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(session.clone()),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("expired URL cycle");

    assert_eq!(result.failed_count, 1);
    assert!(!result.stats.interrupted);
    assert_eq!(result.stats.state_write_failures, 0);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev")
    );
    assert!(!result.can_advance_database_checkpoint());
    let summary = db.get_summary().await.expect("state summary");
    assert_eq!(summary.pending + summary.failed, 1);
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    drop(build_download_config);
    drop(lib_state);
    drop(db);
    drop(inner);
    for cycle in 0..2 {
        let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let library = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            crate::sync_loop::test_support::make_owned_cycle_album(
                &inner,
                "PrimarySync",
                "private",
                Box::new(session.clone()),
            ),
        );
        let builder = make_run_cycle_download_config_builder(download_dir.path(), db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
        assert!(result.can_advance_database_checkpoint(), "{result:?}");
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-tok-next")
        );
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
            body
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending + summary.failed, 0);
    }
    server.verify().await;
}

#[tokio::test]
async fn run_cycle_expired_url_without_durable_retry_preserves_zone_checkpoint() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(0)
        .mount(&server)
        .await;

    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
            .await;
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    inner
        .acquire_lock("inject pending state failure")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_pending BEFORE INSERT ON assets
         BEGIN SELECT RAISE(ABORT, 'simulated pending-row write failure'); END;",
        )
        .unwrap();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let body = b"expired-body";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let session = checkpoint_download_session(
        "master-expired",
        "zone-tok-next",
        &format!("{}/expired.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(session),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("expired URL cycle with failed pending-row write");

    assert_eq!(
        result.failed_count, 2,
        "native state failure leaves selected admission deferred"
    );
    assert_eq!(
        result.stats.failed, 0,
        "no transfer starts before durable admission"
    );
    assert_eq!(result.stats.state_write_failures, 1);
    assert!(result.stats.identity_incomplete);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.bytes_downloaded, 0);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "failed retry admission must prevent the HTTP transfer"
    );
    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "the provider checkpoint must not advance without a durable retry row"
    );
    let summary = inner.get_summary().await.expect("state summary");
    assert_eq!(summary.pending + summary.failed, 0);
    assert_eq!(summary.downloaded, 0);
    assert!(inner.get_pending().await.unwrap().is_empty());
    assert!(inner.get_failed().await.unwrap().is_empty());
}

#[tokio::test]
async fn run_cycle_durable_retry_survives_checkpoint_commit_failure() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(2)
        .mount(&server)
        .await;

    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().unwrap();
    let database = state_dir.path().join("state.db");
    let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    inner
        .acquire_lock("inject checkpoint failure")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_zone_cursor BEFORE INSERT ON metadata
         WHEN NEW.key = 'sync_token:PrimarySync'
         BEGIN SELECT RAISE(ABORT, 'simulated checkpoint commit failure'); END;",
        )
        .unwrap();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let body = b"\xff\xd8\xff\xe0\x00\x10JFIF";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let session = checkpoint_download_session(
        "master-expired",
        "zone-tok-next",
        &format!("{}/expired.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(session.clone()),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("expired URL cycle with checkpoint failure");

    assert_eq!(result.failed_count, 1);
    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "a failed checkpoint commit must retain the replay token"
    );
    let summary = inner.get_summary().await.expect("state summary");
    assert_eq!(
        summary.pending + summary.failed,
        1,
        "durable retry work must survive a failed checkpoint commit"
    );
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    drop(build_download_config);
    drop(lib_state);
    drop(db);
    drop(inner);
    for cycle in 0..3 {
        let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let library = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            crate::sync_loop::test_support::make_owned_cycle_album(
                &inner,
                "PrimarySync",
                "private",
                Box::new(session.clone()),
            ),
        );
        let builder = make_run_cycle_download_config_builder(download_dir.path(), db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, usize::from(cycle == 0), "{result:?}");
        assert_eq!(result.stats.state_write_failures, usize::from(cycle == 0));
        assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
            body
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending + summary.failed, 0);
        if cycle == 0 {
            // Publication completed, so only the installed checkpoint fault can hold this cursor.
            assert!(
                capture.events().iter().any(|event| event
                    .field("error")
                    .is_some_and(|error| error.contains("simulated checkpoint commit failure"))),
                "checkpoint trigger must actually fire after publication"
            );
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("zone-tok-prev")
            );
            inner
                .acquire_lock("remove checkpoint failure")
                .unwrap()
                .execute_batch("DROP TRIGGER fail_zone_cursor;")
                .unwrap();
        } else {
            assert!(result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("zone-tok-next")
            );
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn run_cycle_failed_token_repair_preserves_prior_sqlite_checkpoint() {
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "prior-token")
        .await
        .unwrap();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "PrimarySync",
            "private",
            Box::new(crate::test_helpers::DynamicRecentPhotosSession::new(0).with_token("")),
        ),
    );
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("failed token repair should preserve the active state");

    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
    assert_eq!(result.stats.same_cycle_recovery_successes, 0);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("prior-token")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("old-enum-hash")
    );
    assert!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn run_cycle_legacy_pending_delete_self_heals_without_full_inventory() {
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let database = download_dir.path().join("state.db");
    let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    db.set_metadata("sync_token:PrimarySync", "zone-token-before-legacy-cleanup")
        .await
        .expect("seed zone token");
    let master_record_name = "LEGACY_PENDING_MASTER";
    db.upsert_seen(
        &crate::test_helpers::TestAssetRecord::new(master_record_name)
            .filename("legacy-deleted.jpg")
            .build(),
    )
    .await
    .expect("seed legacy pending row");
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(LegacyPendingDeleteSession {
            master_record_name: Arc::from(master_record_name),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let build_download_config = make_recording_run_cycle_download_config_builder(
        download_dir.path(),
        Arc::clone(&db),
        Arc::clone(&observed_modes),
    );

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("legacy cleanup cycle");

    assert!(result.db_sync_token_advance_safe);
    assert!(
        observed_modes
            .lock()
            .expect("observed modes lock")
            .iter()
            .any(|mode| matches!(mode, download::SyncMode::Incremental { .. }))
    );
    assert!(result.stats.full_enumeration_reason.is_none());
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-token-after-legacy-cleanup")
    );
    let summary = db.get_summary().await.expect("state summary");
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.source_deleted, 1);
    assert_eq!(summary.awaiting_provider_verification, 0);
    drop(build_download_config);
    drop(lib_state);
    drop(db);
    drop(inner);
    let reopened = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    assert_eq!(reopened.get_summary().await.unwrap().source_deleted, 1);
    assert_eq!(
        reopened
            .get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("zone-token-after-legacy-cleanup")
    );
}

#[tokio::test]
async fn run_cycle_multi_zone_status_preserves_an_earlier_checkpoint_hold() {
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let held = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "PrimarySync",
            "private",
            Box::new(crate::test_helpers::DynamicRecentPhotosSession::new(0).with_token("")),
        ),
    );
    let advanced = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "SharedSync-TEST",
            "shared",
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0)
                    .with_zone("SharedSync-TEST")
                    .with_token("shared-token"),
            ),
        ),
    );
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&held, &advanced],
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata("last_checkpoint_status")
            .await
            .expect("read checkpoint status")
            .as_deref(),
        Some("preserved")
    );
    assert_eq!(
        db.get_metadata("last_recovery_action")
            .await
            .expect("read recovery action")
            .as_deref(),
        Some("retry_passes")
    );
}

#[tokio::test]
async fn run_cycle_interrupted_incremental_download_blocks_sync_token_advance() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/cancelled.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 1024]))
        .expect(0)
        .mount(&server)
        .await;
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let database = download_dir.path().join("state.db");
    let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let shutdown_token = CancellationToken::new();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let mut page = full_album_page_with_download(
        "PrimarySync",
        "master-cancelled",
        "zone-tok-new",
        &format!("{}/cancelled.jpg", server.uri()),
        1024,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    );
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        serde_json::json!("_defaultOwner");
    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        crate::sync_loop::test_support::make_owned_cycle_album(
            &inner,
            "PrimarySync",
            "private",
            Box::new(crate::sync_loop::test_support::MutableCaptureSession {
                zone: "PrimarySync",
                token: "zone-tok-new",
                cancel_after_lookup: Some(shutdown_token.clone()),
                records: Arc::new(std::sync::Mutex::new(
                    page["records"].as_array().unwrap().clone(),
                )),
                repair_requests: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        ),
    );
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &shutdown_token,
    )
    .await
    .expect("run cycle");

    // Cancellation interrupts confirmation of one selected identity, before
    // any transfer can fail or publish media.
    assert_eq!(result.failed_count, 1, "{result:?}");
    assert!(result.stats.identity_incomplete);
    assert_eq!(result.stats.failed, 0);
    assert!(
        result.stats.interrupted,
        "cancellation before the download pass must be visible in cycle stats"
    );
    assert!(
        !result.db_sync_token_advance_safe,
        "database precheck token must not advance after an interrupted download cycle"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "interrupted cycle must leave the old zone token in place for replay"
    );
    assert!(
        !download_dir
            .path()
            .join(run_cycle_expected_date_dir())
            .join("photo.jpg")
            .exists(),
        "test must not pass by completing the download before cancellation"
    );
    assert!(inner.get_downloaded_page(0, 10).await.unwrap().is_empty());
    server.verify().await;
    drop(build_download_config);
    drop(states);
    drop(lib_state);
    drop(db);
    drop(inner);
    let reopened = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
    assert_eq!(
        reopened
            .get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("zone-tok-prev")
    );
}
#[tokio::test]
async fn checkpoint_pilot_holds_preserve_file_backed_state_across_cycles() {
    for hold in ["dry_run", "missing_token", "stale_plan"] {
        let mut config = make_run_cycle_config();
        config.runtime.dry_run = hold == "dry_run";
        let state_dir = tempfile::tempdir().unwrap();
        let database = state_dir.path().join("state.db");
        let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "prior-token")
            .await
            .unwrap();
        let download_dir = tempfile::tempdir().unwrap();
        let existing_path = download_dir.path().join("unrelated.jpg");
        std::fs::write(&existing_path, b"existing media must not change").unwrap();
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        drop(db);
        drop(inner);
        let controls = if config.runtime.dry_run {
            download::DownloadControls::dry_run_hidden()
        } else {
            download::DownloadControls::download_hidden()
        };
        for _ in 0..2 {
            let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
            let db: Arc<dyn download::DownloadStore> = inner.clone();
            let build_download_config =
                make_run_cycle_download_config_builder(download_dir.path(), db.clone());
            let mut library = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                crate::sync_loop::test_support::make_owned_cycle_album(
                    &inner,
                    "PrimarySync",
                    "private",
                    Box::new(
                        crate::test_helpers::DynamicRecentPhotosSession::new(0).with_token(
                            if hold == "missing_token" {
                                ""
                            } else {
                                "candidate-token"
                            },
                        ),
                    ),
                ),
            );
            library.plan_is_stale = hold == "stale_plan";
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                &build_download_config,
                controls,
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!result.db_sync_token_advance_safe, "{hold}");
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("prior-token"),
                "{hold}"
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("old-enum-hash"),
                "{hold}"
            );
            assert_eq!(
                std::fs::read(&existing_path).unwrap(),
                b"existing media must not change"
            );
            assert_eq!(std::fs::read_dir(download_dir.path()).unwrap().count(), 1);
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), 0);
        }
    }
}

#[tokio::test]
async fn full_checkpoint_pilot_holds_durable_transfer_failure_until_publication() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(410))
        // Initial transfer and the bounded current-record URL refresh.
        .expect(2)
        .mount(&server)
        .await;
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().unwrap();
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let download_dir = tempfile::tempdir().unwrap();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let body = b"\xff\xd8\xff\xe0\x00\x10JFIF";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let mut page = full_album_page_with_download(
        "PrimarySync",
        "master-full-expired",
        "full-token",
        &format!("{}/expired.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        serde_json::json!("_defaultOwner");
    let session = crate::sync_loop::test_support::MutableCaptureSession {
        zone: "PrimarySync",
        token: "full-token",
        cancel_after_lookup: None,
        records: Arc::new(std::sync::Mutex::new(
            page["records"].as_array().unwrap().clone(),
        )),
        repair_requests: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let album = crate::sync_loop::test_support::make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(session.clone()),
    );
    let library =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let builder = make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        &builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 1);
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(!result.can_advance_database_checkpoint());
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        None,
    );
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending + summary.failed, 1);
    assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    drop(builder);
    drop(library);
    drop(db);
    drop(inner);
    for cycle in 0..2 {
        let inner =
            crate::sync_loop::test_support::open_owned_cycle_db(&state_dir.path().join("state.db"))
                .await;
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let library = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            crate::sync_loop::test_support::make_owned_cycle_album(
                &inner,
                "PrimarySync",
                "private",
                Box::new(session.clone()),
            ),
        );
        let builder = make_run_cycle_download_config_builder(download_dir.path(), db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
        assert!(result.can_advance_database_checkpoint(), "{result:?}");
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("full-token")
        );
        let rows = inner.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
            body
        );
        assert_eq!(inner.get_summary().await.unwrap().pending, 0);
        assert_eq!(inner.get_summary().await.unwrap().failed, 0);
    }
    server.verify().await;
}

#[derive(Clone)]
struct ValidatedPageCycleSession {
    phase: u8,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for ValidatedPageCycleSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        use serde_json::json;
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        if url.contains("/changes/zone?") {
            let token = body["zones"][0]["syncToken"].as_str().unwrap_or("");
            self.requests.lock().unwrap().push(token.to_owned());
            let (records, successor, more) = match (self.phase, token) {
                (0, "saved") => (
                    json!([{ "recordName": "unrelated-source", "recordType": null, "deleted": true }]),
                    "accepted-page",
                    true,
                ),
                (0, _) => {
                    return Ok(json!({"zones": [{
                        "zoneID": {"zoneName": "WrongZone"}, "records": [],
                        "syncToken": "rejected-successor", "moreComing": false
                    }]}));
                }
                (1, _) => (json!([]), "saved", false),
                (2, _) => (
                    json!([{"recordName": "DEBT", "recordType": null, "deleted": true}]),
                    "recovered",
                    false,
                ),
                _ => (json!([]), "recovered", false),
            };
            return Ok(
                json!({"zones": [{"zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "records": records, "syncToken": successor, "moreComing": more}]}),
            );
        }
        if url.contains("/records/lookup?") {
            return Ok(if self.phase < 2 {
                json!({"records": []})
            } else {
                json!({"records": [{
                    "recordName": "DEBT", "serverErrorCode": "UNKNOWN_ITEM"
                }]})
            });
        }
        if self.phase == 0 {
            anyhow::bail!("No authoritative fallback fixture");
        }
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response(u64::from(self.phase == 1)));
        }
        if self.phase == 1 {
            anyhow::bail!(
                "Current inventory interrupted before selected membership was established"
            );
        }
        Ok(json!({"records": [], "syncToken": if self.phase == 1 {"saved"} else {"recovered"}}))
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn run_cycle_rejected_zone_page_preserves_cursor_debt_and_media_across_restart() {
    let config = make_run_cycle_config();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("historical.jpg");
    tokio::fs::write(&media, b"historical media remains intact")
        .await
        .unwrap();
    {
        let db = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        db.set_metadata("sync_token:PrimarySync", "saved")
            .await
            .unwrap();
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .unwrap();
        db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("asset-HISTORY").build())
            .await
            .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "asset-HISTORY",
            "original",
            &media,
            "historical-checksum",
            None,
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-HISTORY", "master-HISTORY")
            .await
            .unwrap();
        db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("DEBT").build())
            .await
            .unwrap();
    }
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    for phase in 0..4 {
        let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let library = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            crate::sync_loop::test_support::make_owned_cycle_album(
                &inner,
                "PrimarySync",
                "private",
                Box::new(ValidatedPageCycleSession {
                    phase,
                    requests: requests.clone(),
                }),
            ),
        );
        if phase == 1 {
            inner
                .set_metadata(&state::unresolved_identity_key("PrimarySync"), "1")
                .await
                .unwrap();
            let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
            crate::sync_loop::precheck::include_pending_local_work(
                &mut precheck,
                inner.as_ref(),
                Some(&config.metadata),
                std::slice::from_ref(&library),
            )
            .await;
            assert!(precheck.should_sync_zone("PrimarySync"));
        }
        let builder = make_run_cycle_download_config_builder(dir.path(), db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await;
        let summary = inner.get_summary().await.unwrap();
        assert_eq!(
            tokio::fs::read(&media).await.unwrap(),
            b"historical media remains intact"
        );
        assert_eq!(summary.downloaded, 1, "phase {phase}");
        let cursor = inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .unwrap();
        if phase == 0 {
            let result = result.unwrap();
            assert_eq!(result.failed_count, 1, "{result:?}");
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(
                result.stats.sync_token_blocked_reason,
                Some("icloud_changes_zone_name_mismatch")
            );
            assert_eq!(cursor, "saved");
            assert_eq!(summary.pending + summary.failed, 1);
            assert_eq!(summary.source_deleted, 0);
            assert_eq!(*requests.lock().unwrap(), ["saved", "accepted-page"]);
            continue;
        }
        let result = result.unwrap();
        assert_eq!(result.stats.downloaded, 0, "phase {phase}");
        if phase < 2 {
            assert_eq!(cursor, "saved", "phase {phase}");
            assert_eq!(
                summary.pending + summary.failed,
                1,
                "phase {phase}: {summary:?}"
            );
            assert_eq!(summary.source_deleted, 0);
            assert!(
                !result.can_advance_database_checkpoint(),
                "phase {phase}: {result:?}"
            );
        } else {
            assert_eq!(cursor, "recovered", "phase {phase}");
            assert_eq!(summary.pending, 0);
            assert_eq!(summary.source_deleted, 1);
            assert!(result.db_sync_token_advance_safe, "phase {phase}");
        }
        if phase == 3 {
            assert!(result.stats.full_enumeration_reason.is_none());
        }
    }
}

#[derive(Clone)]
struct DownloadReplayPageSession {
    phase: u8,
    earlier: Vec<serde_json::Value>,
    debt: Vec<serde_json::Value>,
    rejected: Vec<serde_json::Value>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for DownloadReplayPageSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        use serde_json::json;
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        if url.contains("/changes/zone?") {
            let token = body["zones"][0]["syncToken"].as_str().unwrap_or("");
            self.requests.lock().unwrap().push(format!("delta:{token}"));
            let (records, successor, more) = match (self.phase, token) {
                (0, "saved") => (self.earlier.clone(), "accepted-page", true),
                (0, "accepted-page") => {
                    let mut records = self.rejected.clone();
                    records.push(
                        json!({"recordName": "private-error", "recordType": "FutureType",
                        "serverErrorCode": "UNKNOWN_ITEM", "reason": "private-reason"}),
                    );
                    (records, "rejected-successor", false)
                }
                (1, "saved") => (vec![], "saved", false),
                (2, "saved") => (self.earlier.clone(), "accepted-page", true),
                (2, "accepted-page") => (self.debt.clone(), "recovered", false),
                (3 | 4, "recovered") => (vec![], "recovered", false),
                (1, "") => (vec![], "saved", false),
                (2..=4, "") => (
                    self.earlier.iter().chain(&self.debt).cloned().collect(),
                    "recovered",
                    false,
                ),
                _ => anyhow::bail!("Unexpected replay phase or cursor"),
            };
            return Ok(
                json!({"zones": [{"zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "records": records, "syncToken": successor, "moreComing": more}]}),
            );
        }
        if url.contains("/records/lookup?") {
            self.requests.lock().unwrap().push("lookup".to_owned());
            let records: Vec<_> = if self.phase < 2 {
                Vec::new()
            } else {
                self.earlier
                    .iter()
                    .chain(&self.debt)
                    .filter(|record| {
                        body["records"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|requested| requested["recordName"] == record["recordName"])
                    })
                    .cloned()
                    .collect()
            };
            return Ok(json!({"records": records}));
        }
        // A malformed delta must not be rescued by invented authoritative
        // inventory. Later recent-selection queries see the same valid work.
        anyhow::ensure!(self.phase > 0, "No authoritative fallback fixture");
        if url.contains("/internal/records/query/batch?") {
            self.requests.lock().unwrap().push("count".to_owned());
            return Ok(album_count_response(if self.phase == 1 { 0 } else { 2 }));
        }
        if url.contains("/records/query?") {
            self.requests.lock().unwrap().push("query".to_owned());
            let offset = body["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if self.phase == 1 || offset > 0 {
                vec![]
            } else {
                self.earlier.iter().chain(&self.debt).cloned().collect()
            };
            return Ok(
                json!({"records": records, "syncToken": if self.phase == 1 {"saved"} else {"recovered"}}),
            );
        }
        anyhow::bail!("Unexpected provider endpoint");
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn run_cycle_rejected_page_replays_downloads_exactly_once_across_restart() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    for collecting in [false, true] {
        let server = wiremock::MockServer::start().await;
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
        for (name, transfers) in [("earlier", 1), ("debt", 1), ("rejected", 0)] {
            Mock::given(method("GET"))
                .and(path(format!("/{name}.jpg")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
                .expect(transfers)
                .mount(&server)
                .await;
        }
        let records = |name: &str| {
            let mut page = full_album_page_with_download(
                "PrimarySync",
                &format!("master-{name}"),
                "unused",
                &format!("{}/{name}.jpg", server.uri()),
                bytes.len() as u64,
                &checksum,
            );
            page["records"][0]["fields"]["filenameEnc"]["value"] = serde_json::json!(
                base64::engine::general_purpose::STANDARD.encode(format!("{name}.jpg"))
            );
            page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                serde_json::json!("_defaultOwner");
            page["records"].as_array().unwrap().clone()
        };
        let config = make_run_cycle_config();
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("state.db");
        let media_dir = dir.path().join("media");
        tokio::fs::create_dir(&media_dir).await.unwrap();
        let historical = media_dir.join("historical.jpg");
        let history_bytes = b"historical media remains intact";
        tokio::fs::write(&historical, history_bytes).await.unwrap();
        let history_snapshot;
        {
            let db = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
            db.set_metadata("sync_token:PrimarySync", "saved")
                .await
                .unwrap();
            db.set_metadata(
                ENUM_CONFIG_HASH_KEY,
                &download::compute_config_hash(&config),
            )
            .await
            .unwrap();
            db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("asset-HISTORY").build())
                .await
                .unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "asset-HISTORY",
                "original",
                &historical,
                "historical-checksum",
                None,
            )
            .await
            .unwrap();
            db.upsert_asset_master_mapping("PrimarySync", "asset-HISTORY", "master-HISTORY")
                .await
                .unwrap();
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new("asset-master-debt")
                    .filename("debt.jpg")
                    .size(bytes.len() as u64)
                    .checksum(&checksum)
                    .build(),
            )
            .await
            .unwrap();
            db.upsert_asset_master_mapping("PrimarySync", "asset-master-debt", "master-debt")
                .await
                .unwrap();
            db.set_metadata(&state::unresolved_identity_key("PrimarySync"), "1")
                .await
                .unwrap();
            history_snapshot = db.get_downloaded_page(0, 10).await.unwrap().remove(0);
        }
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let mut recovered_snapshot = None;
        for phase in 0..5 {
            let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
            let db: Arc<dyn download::DownloadStore> = inner.clone();
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut library = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                crate::sync_loop::test_support::make_owned_cycle_album(
                    &inner,
                    "PrimarySync",
                    "private",
                    Box::new(DownloadReplayPageSession {
                        phase,
                        earlier: records("earlier"),
                        debt: records("debt"),
                        rejected: records("rejected"),
                        requests: requests.clone(),
                    }),
                ),
            );
            if collecting {
                // Multiple passes enter the production collecting strategy;
                // the primary pass still selects every original input record.
                let mut peer = library.plan.passes[0].clone();
                peer.exclude_ids = Arc::new(
                    [
                        "asset-master-earlier".into(),
                        "asset-master-debt".into(),
                        "asset-master-rejected".into(),
                    ]
                    .into_iter()
                    .collect(),
                );
                library.plan.passes.push(peer);
            }
            if phase == 1 {
                let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
                crate::sync_loop::precheck::include_pending_local_work(
                    &mut precheck,
                    inner.as_ref(),
                    Some(&config.metadata),
                    std::slice::from_ref(&library),
                )
                .await;
                assert!(precheck.should_sync_zone("PrimarySync"));
            }
            let builder = make_run_cycle_download_config_builder_with_options(
                &media_dir,
                db.clone(),
                RunCycleDownloadConfigOptions::default(),
            );
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            if phase == 0 {
                assert!(!result.can_advance_database_checkpoint(), "{result:?}");
                assert_eq!(result.failed_count, 1, "{result:?}");
                assert_eq!(
                    result.stats.sync_token_blocked_reason,
                    Some("icloud_changes_record_error")
                );
            }
            drop(builder);
            drop(db);
            drop(inner);

            // Read the committed outcomes through a new SQLite connection.
            let db = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
            let summary = db.get_summary().await.unwrap();
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            let history = rows
                .iter()
                .find(|row| row.id.as_ref() == "asset-HISTORY")
                .unwrap();
            assert_eq!(history.local_path, history_snapshot.local_path);
            assert_eq!(history.local_checksum, history_snapshot.local_checksum);
            assert_eq!(history.downloaded_at, history_snapshot.downloaded_at);
            assert_eq!(history.created_at, history_snapshot.created_at);
            assert_eq!(tokio::fs::read(&historical).await.unwrap(), history_bytes);
            for row in rows.iter().filter(|row| row.id.as_ref() != "asset-HISTORY") {
                assert_eq!(
                    tokio::fs::read(row.local_path.as_ref().unwrap())
                        .await
                        .unwrap(),
                    bytes
                );
                assert_eq!(
                    row.local_checksum.as_deref(),
                    Some(format!("{:x}", Sha256::digest(bytes)).as_str())
                );
            }
            assert!(
                !db.get_all_known_ids()
                    .await
                    .unwrap()
                    .iter()
                    .any(|(_, id)| id.contains("rejected"))
            );
            let cursor = db
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .unwrap();
            if phase < 2 {
                assert_eq!(cursor, "saved", "collecting={collecting} phase={phase}");
                assert_eq!(summary.pending + summary.failed, 1);
                assert_eq!(result.stats.downloaded, 0);
                if phase == 1 {
                    assert!(
                        result.can_advance_database_checkpoint(),
                        "empty current selection is complete: {result:?}"
                    );
                    assert_eq!(result.failed_count, 0);
                    assert_eq!(result.stats.assets_seen, 0);
                    assert!(!result.stats.identity_incomplete);
                }
                assert!(
                    server.received_requests().await.unwrap().is_empty(),
                    "no current publication is admitted before phase 2"
                );
                assert_eq!(summary.source_deleted, 0);
                assert_eq!(
                    summary.downloaded, 1,
                    "rejected source inventory must not publish partial work"
                );
                let mut pending = db.get_pending().await.unwrap();
                pending.extend(db.get_failed().await.unwrap());
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].id.as_ref(), "asset-master-debt");
                assert_eq!(pending[0].checksum.as_ref(), checksum);
                assert_eq!(pending[0].filename.as_ref(), "debt.jpg");
                assert_eq!(pending[0].size_bytes, bytes.len() as u64);
                assert_eq!(
                    db.get_master_record_name_for_asset("PrimarySync", "asset-master-debt")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("master-debt"),
                    "empty current selection must preserve historical identity evidence"
                );
                // Rejected input retains the replay marker. A complete empty
                // current selection consumes that scheduling marker, not the
                // unresolved historical row or its provider identity.
                assert_eq!(
                    db.get_metadata(&state::unresolved_identity_key("PrimarySync"))
                        .await
                        .unwrap()
                        .as_deref(),
                    if phase == 0 { Some("1") } else { None }
                );
            } else {
                assert_eq!(cursor, "recovered");
                assert_eq!(summary.pending + summary.failed, 0);
                assert_eq!(
                    summary.source_deleted, 0,
                    "recovery must download, not delete"
                );
                assert_eq!(summary.downloaded, 3);
                assert_eq!(result.stats.downloaded, if phase == 2 { 2 } else { 0 });
                assert_eq!(
                    server.received_requests().await.unwrap().len(),
                    2,
                    "current recovery publishes each source once across quiet reopens"
                );
                assert!(result.can_advance_database_checkpoint(), "{result:?}");
                assert!(
                    db.get_metadata(&state::unresolved_identity_key("PrimarySync"))
                        .await
                        .unwrap()
                        .is_none()
                );
                if phase == 2 {
                    recovered_snapshot = Some(rows.clone());
                }
                for row in &rows {
                    let saved = recovered_snapshot
                        .as_ref()
                        .unwrap()
                        .iter()
                        .find(|saved| saved.id == row.id)
                        .unwrap();
                    assert_eq!(row.local_path, saved.local_path);
                    assert_eq!(row.local_checksum, saved.local_checksum);
                    assert_eq!(row.downloaded_at, saved.downloaded_at);
                }
            }
            if phase == 0 || phase == 2 {
                assert_eq!(
                    &requests.lock().unwrap()[..2],
                    ["delta:saved", "delta:accepted-page"]
                );
            }
            if phase > 2 {
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(*requests.lock().unwrap(), ["delta:recovered"]);
                assert!(result.stats.full_enumeration_reason.is_none());
                let mut directories = vec![media_dir.clone()];
                let mut files = Vec::new();
                while let Some(directory) = directories.pop() {
                    for entry in std::fs::read_dir(directory).unwrap() {
                        let entry = entry.unwrap();
                        if entry.file_type().unwrap().is_dir() {
                            directories.push(entry.path());
                        } else {
                            files.push(entry.path());
                        }
                    }
                }
                let mut expected: Vec<_> = rows
                    .iter()
                    .map(|row| row.local_path.clone().unwrap())
                    .collect();
                files.sort();
                expected.sort();
                assert_eq!(files, expected, "no duplicate or orphaned publication");
            }
        }
        server.verify().await;
    }
}

#[tokio::test]
async fn account_adoption_reopens_owned_state_and_runs_two_production_quiet_cycles() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let config = make_run_cycle_config();
    let directory = tempfile::tempdir().unwrap();
    let username = config.auth.username.as_str();
    let source = directory.path().join(format!(
        "{}.db",
        crate::auth::session::sanitize_username(username)
    ));
    let destination = directory
        .path()
        .join(format!("{}.db", crate::account::namespace(username, "com")));
    let media = directory.path().join("kept.jpg");
    let bytes = b"preexisting adopted media stays exact";
    std::fs::write(&media, bytes).unwrap();
    let provider_checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let local_checksum = format!("{:x}", Sha256::digest(bytes));
    {
        let legacy = state::SqliteStateDb::open(&source).await.unwrap();
        legacy
            .set_metadata("sync_token:PrimarySync", "adopted-cursor")
            .await
            .unwrap();
        legacy
            .set_metadata(
                ENUM_CONFIG_HASH_KEY,
                &download::compute_config_hash(&config),
            )
            .await
            .unwrap();
        legacy
            .upsert_seen(
                &crate::test_helpers::TestAssetRecord::new("adopted")
                    .filename("kept.jpg")
                    .size(bytes.len() as u64)
                    .checksum(&provider_checksum)
                    .build(),
            )
            .await
            .unwrap();
        legacy
            .mark_downloaded(
                "PrimarySync",
                "adopted",
                "original",
                &media,
                &local_checksum,
                Some(&provider_checksum),
            )
            .await
            .unwrap();
        legacy
            .upsert_asset_master_mapping("PrimarySync", "adopted", "master-adopted")
            .await
            .unwrap();
    }
    let source_bytes = std::fs::read(&source).unwrap();
    let authenticated: crate::auth::AccountLoginResponse = serde_json::from_value(
        serde_json::json!({"dsInfo":{"dsid":"synthetic-adoption-provider"}}),
    )
    .unwrap();
    let owner =
        state::db::account::AccountOwner::authenticated(username, "com", &authenticated).unwrap();
    state::db::account::adopt_legacy(&source, &destination, &owner, username, "com")
        .await
        .unwrap();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    shared
        .write()
        .await
        .bind_principal(&authenticated, None)
        .unwrap();
    // Adoption retains source history, but current selection still needs one
    // real inventory before two reopened cycles can prove quiet execution.
    for cycle in 0..3 {
        assert_eq!(
            state::db::account::state_path(directory.path(), username, "com")
                .await
                .unwrap(),
            destination
        );
        let inner = Arc::new(
            state::SqliteStateDb::open_owned(&destination, &owner)
                .await
                .unwrap(),
        );
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        // The quiet synthetic source reports the retained cursor, rather than
        // creating an artificial full-scan anchor or replacing historical work.
        let album = crate::sync_loop::test_support::make_captured_cycle_album(
            "PrimarySync",
            crate::icloud::photos::inbox::ShadowCapture::new(inner.clone(), owner.clone(), "com"),
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0)
                    .with_token("adopted-cursor"),
            ),
        );
        let library =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let builder = make_run_cycle_download_config_builder(directory.path(), db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.stats.downloaded, 0, "cycle {cycle}");
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        if cycle > 0 {
            assert!(
                result.stats.full_enumeration_reason.is_none(),
                "cycle {cycle}: {result:?}"
            );
        }
        assert!(
            result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("adopted-cursor")
        );
        assert_eq!(inner.get_summary().await.unwrap().downloaded, 1);
        let conn = inner.acquire_lock("adopted completion receipt").unwrap();
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT local_checksum FROM asset_metadata_paths WHERE id='adopted'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            local_checksum
        );
        assert_eq!(conn.query_row::<String,_,_>("SELECT master_record_name FROM asset_master_mappings WHERE asset_record_name='adopted'",[],|r|r.get(0)).unwrap(),"master-adopted");
        drop(conn);
        drop(builder);
        drop(library);
        drop(db);
        drop(inner);
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
    }
}

#[tokio::test]
async fn run_cycle_shadow_transaction_preserves_cursor_debt_and_media_across_restart() {
    let config = make_run_cycle_config();
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-provider"}}))
            .unwrap(),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("historical.jpg");
    tokio::fs::write(&media, b"historical media remains intact")
        .await
        .unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&database, &owner)
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "saved")
            .await
            .unwrap();
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .unwrap();
        db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("asset-HISTORY").build())
            .await
            .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "asset-HISTORY",
            "original",
            &media,
            "historical-checksum",
            None,
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-HISTORY", "master-HISTORY")
            .await
            .unwrap();
        db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("DEBT").build())
            .await
            .unwrap();
    }
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    for phase in 0..4 {
        let inner = Arc::new(
            state::SqliteStateDb::open_owned(&database, &owner)
                .await
                .unwrap(),
        );
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        if phase == 0 {
            inner.acquire_lock("inject shadow page failure").unwrap().execute_batch(
                "CREATE TRIGGER shadow_cycle_fault AFTER INSERT ON provider_shadow_pages BEGIN SELECT RAISE(ABORT,'synthetic capture fault'); END").unwrap();
        } else if phase == 1 {
            inner
                .acquire_lock("remove injected capture fault")
                .unwrap()
                .execute_batch("DROP TRIGGER shadow_cycle_fault")
                .unwrap();
        }
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(inner.clone(), owner.clone(), "com");
        let album = crate::sync_loop::test_support::make_captured_cycle_album(
            "PrimarySync",
            capture,
            Box::new(ValidatedPageCycleSession {
                phase,
                requests: requests.clone(),
            }),
        );
        let library =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        if phase == 1 {
            inner
                .set_metadata(&state::unresolved_identity_key("PrimarySync"), "1")
                .await
                .unwrap();
            let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
            crate::sync_loop::precheck::include_pending_local_work(
                &mut precheck,
                inner.as_ref(),
                Some(&config.metadata),
                std::slice::from_ref(&library),
            )
            .await;
            assert!(precheck.should_sync_zone("PrimarySync"));
        }
        let builder = make_run_cycle_download_config_builder(dir.path(), db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await;
        if phase == 0 {
            let error =
                result.expect_err("capture refusal must propagate, not run a rank fallback");
            assert!(
                format!("{error:#}").contains("synthetic capture fault"),
                "{error:#}"
            );
            assert_eq!(*requests.lock().unwrap(), ["saved"]);
            let summary = inner.get_summary().await.unwrap();
            assert_eq!(summary.downloaded, 1);
            assert_eq!(summary.pending + summary.failed, 1);
            assert_eq!(summary.source_deleted, 0);
            assert_eq!(
                inner
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("saved")
            );
            assert_eq!(
                tokio::fs::read(&media).await.unwrap(),
                b"historical media remains intact"
            );
            let conn = inner.acquire_lock("no partial capture or receipt").unwrap();
            for table in [
                "provider_shadow_pages",
                "provider_shadow_records",
                "provider_shadow_receipts",
            ] {
                assert_eq!(
                    conn.query_row::<i64, _, _>(
                        &format!("SELECT count(*) FROM {table}"),
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                    0
                );
            }
            continue;
        }
        let result = result.unwrap();
        let summary = inner.get_summary().await.unwrap();
        assert_eq!(
            tokio::fs::read(&media).await.unwrap(),
            b"historical media remains intact"
        );
        assert_eq!(summary.downloaded, 1, "phase {phase}");
        assert_eq!(result.stats.downloaded, 0, "phase {phase}");
        let cursor = inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .unwrap();
        if phase < 2 {
            assert_eq!(cursor, "saved", "phase {phase}");
            assert_eq!(
                summary.pending + summary.failed,
                1,
                "phase {phase}: {summary:?}"
            );
            assert_eq!(summary.source_deleted, 0);
            assert!(
                !result.can_advance_database_checkpoint(),
                "phase {phase}: {result:?}"
            );
        } else {
            assert_eq!(cursor, "recovered", "phase {phase}");
            assert_eq!(summary.pending, 0);
            assert_eq!(summary.source_deleted, 1);
            assert!(result.db_sync_token_advance_safe, "phase {phase}");
        }
        if phase == 3 {
            assert!(result.stats.full_enumeration_reason.is_none());
            assert_eq!(*requests.lock().unwrap(), ["recovered"]);
            // Only the page carrying the DEBT deletion survives the restart.
            // Both empty terminal pages are reclaimed after capture.
            let conn = inner.acquire_lock("retained recovery page").unwrap();
            assert_eq!(
                conn.prepare("SELECT request_cursor, successor FROM provider_shadow_pages")
                    .unwrap()
                    .query_map([], |row| Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?
                    )))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap(),
                [("saved".to_owned(), "recovered".to_owned())]
            );
        }
    }
}
