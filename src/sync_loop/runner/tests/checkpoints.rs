use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{
    ENUM_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, SYNC_TOKEN_PREFIX, determine_sync_mode,
    run_cycle, should_store_sync_token, should_store_sync_token_for_cycle,
};
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, MetadataSetFailure, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page, full_album_page_with_download, make_empty_full_album,
    make_full_album_with_boxed_session, make_full_album_with_session,
    make_recording_run_cycle_download_config_builder, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle, make_state_db,
    media_without_photo_downloads, run_cycle_expected_date_dir, run_full_cycle_with_album,
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
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/changes/zone?") {
            return Ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-token-after-legacy-cleanup",
                    "moreComing": false,
                    "records": []
                }]
            }));
        }
        if url.contains("/records/lookup?") {
            return Ok(serde_json::json!({
                "records": [{
                    "recordName": self.master_record_name.as_ref(),
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                }]
            }));
        }
        Ok(serde_json::json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

fn make_one_photo_incremental_album_for_zone(
    zone: &str,
    zone_sync_token: &str,
) -> crate::icloud::photos::PhotoAlbum {
    make_one_photo_incremental_album_with_download(
        zone,
        zone_sync_token,
        "https://p01.icloud-content.com/photo.jpg",
        1024,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    )
}

fn make_one_photo_incremental_album_with_download(
    zone: &str,
    zone_sync_token: &str,
    download_url: &str,
    size: u64,
    checksum: &str,
) -> crate::icloud::photos::PhotoAlbum {
    use serde_json::json;
    let page = full_album_page_with_download(
        zone,
        &format!("master-{zone}"),
        zone_sync_token,
        download_url,
        size,
        checksum,
    );
    let records = page
        .get("records")
        .expect("full album page records")
        .clone();

    make_full_album_with_session(
        zone,
        crate::test_helpers::MockPhotosSession::new().ok(json!({
            "zones": [{
                "zoneID": {"zoneName": zone, "ownerRecordName": "_defaultOwner"},
                "syncToken": zone_sync_token,
                "moreComing": false,
                "records": records
            }]
        })),
    )
}

#[tokio::test]
async fn run_cycle_recent_exact_inventory_stores_zone_token() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let mut config = make_run_cycle_config();
    config.filters.recent = Some(40);
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let session = crate::test_helpers::DynamicRecentPhotosSession::new(40)
        .with_filename_prefix("cycle-recent")
        .with_token("zone-tok-recent");
    let album = make_full_album_with_boxed_session("PrimarySync", Box::new(session));
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            recent: Some(40),
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
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

    let result = run_full_cycle_with_album(
        make_empty_full_album(""),
        false,
        download::DownloadControls::download_hidden(),
    )
    .await;

    assert_eq!(result.failed_count, 0);
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
    let db = make_state_db();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "stale-enum-config")
        .await
        .expect("seed stale enum config hash");

    let mut page = full_album_page("PrimarySync", "", "zone-tok-new");
    page["records"][0]["recordName"] = serde_json::json!("");
    page["records"][1]["recordName"] = serde_json::json!("");
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(page),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
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
    let mut config = make_run_cycle_config();
    config.filters.recent = Some(20);
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
        .with_filename_prefix("watch-recent")
        .with_token("zone-tok-watch");
    let album = make_full_album_with_boxed_session("PrimarySync", Box::new(session));
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            recent: Some(20),
            ..RunCycleDownloadConfigOptions::default()
        },
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
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
        false,
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
}

/// Retry work is rehydrated independently from source enumeration, so a
/// stored provider checkpoint remains usable during `--retry-failed`.
#[tokio::test]
async fn determine_sync_mode_retry_failed_with_token_returns_incremental() {
    let db = make_state_db();
    let sync_token_key = "sync_token:PrimarySync";
    // Pre-populate a stored token so we can verify it is ignored.
    db.set_metadata(sync_token_key, "stored-token-abc")
        .await
        .expect("set token");

    let mode = determine_sync_mode(
        true, // is_retry_failed
        1,
        Some(db.as_ref()),
        sync_token_key,
        "PrimarySync",
    )
    .await;

    assert!(
        matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
        "retry-failed should keep source tracking incremental, got {mode:?}"
    );
}

/// Retry-failed must neither consume nor clear the stored provider token.
#[tokio::test]
async fn determine_sync_mode_retry_failed_does_not_consume_or_clear_stored_token() {
    let db = make_state_db();
    let sync_token_key = "sync_token:PrimarySync";
    db.set_metadata(sync_token_key, "stored-token-abc")
        .await
        .expect("set token");

    let retry_mode =
        determine_sync_mode(true, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
    assert!(
        matches!(retry_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
        "retry-failed should use the stored token, got {retry_mode:?}"
    );

    let normal_mode =
        determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
    assert!(
        matches!(normal_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
        "normal sync should still use the stored token after retry-failed, got {normal_mode:?}"
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

    let mode =
        determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;

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
    let mode =
        determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
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

    let mode =
        determine_sync_mode(false, 1, Some(&db), "sync_token:PrimarySync", "PrimarySync").await;

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
        false,
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
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .expect("file-backed state"),
    );
    db.set_metadata("sync_token:PrimarySync", "primary-prev")
        .await
        .expect("seed primary token");
    db.set_metadata("sync_token:SharedSync-TEST", "shared-prev")
        .await
        .expect("seed shared token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let clean_state =
        make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "primary-new");
    let mut stale_state = make_run_cycle_library_state(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        "shared-new",
    );
    stale_state.plan_is_stale = true;
    let states = vec![&clean_state, &stale_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
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

#[tokio::test]
async fn run_cycle_stale_plan_blocks_database_precheck_token() {
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .expect("file-backed state"),
    );
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let mut lib_state =
        make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "zone-tok-new");
    lib_state.plan_is_stale = true;
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
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
    let inner = make_state_db();
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        Arc::clone(&inner),
        MetadataSetFailure::Prefix(SYNC_TOKEN_PREFIX),
        "simulated sync-token write failure",
    ));
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state =
        make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "zone-tok-new");
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
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
    let inner: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .expect("file-backed state"),
    );
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(
            Arc::clone(&inner),
            "simulated mark_downloaded failure",
        )
        .with_mark_downloaded_failure(),
    );
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

    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(full_album_page_with_download(
                "PrimarySync",
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
        false,
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
    let inner = Arc::new(state::SqliteStateDb::open_in_memory().expect("state db"));
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let media_path = download_dir.path().join("TestAlbum/cGhvdG8uanBn.JPG");
    let page = full_album_page_with_download(
        "PrimarySync",
        "master-PrimarySync",
        "zone-tok-new",
        &format!("{}/photo.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(page.clone())
            .ok(album_count_response(1))
            .ok(page.clone())
            .ok(album_count_response(1))
            .ok(page),
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
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run failed grouping cycle");

    assert_eq!(failed.failed_count, 1, "cycle stats: {:?}", failed.stats);
    assert_eq!(
        failed.stats.state_write_failures, 1,
        "cycle stats: {:?}",
        failed.stats
    );
    assert_eq!(failed.stats.downloaded, 1);
    assert_eq!(
        std::fs::read(&media_path).expect("read landed media"),
        body,
        "the media must land even though grouping state is not durable"
    );
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
    for cycle in 2..=3 {
        let recovered = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("grouping replay cycle {cycle} failed: {error}"));

        assert_eq!(recovered.failed_count, 0);
        assert_eq!(recovered.stats.state_write_failures, 0);
        assert_eq!(
            recovered.stats.downloaded, 0,
            "cycle {cycle} must not redownload landed media"
        );
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
        std::fs::read(&media_path).expect("read stable media"),
        body,
        "grouping replay must not change landed media"
    );
}

#[tokio::test]
async fn run_cycle_durable_expired_url_failure_advances_zone_checkpoint() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(1)
        .mount(&server)
        .await;

    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .expect("file-backed state"),
    );
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let body = b"expired-body";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let album = make_one_photo_incremental_album_with_download(
        "PrimarySync",
        "zone-tok-next",
        &format!("{}/expired.jpg", server.uri()),
        body.len() as u64,
        &checksum,
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
        false,
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
        Some("zone-tok-next")
    );
    let summary = db.get_summary().await.expect("state summary");
    assert_eq!(summary.pending + summary.failed, 1);
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
    let inner: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .expect("file-backed state"),
    );
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(
            Arc::clone(&inner),
            "simulated pending-row write failure",
        )
        .with_upsert_seen_failure(),
    );
    let body = b"expired-body";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let album = make_one_photo_incremental_album_with_download(
        "PrimarySync",
        "zone-tok-next",
        &format!("{}/expired.jpg", server.uri()),
        body.len() as u64,
        &checksum,
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
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("expired URL cycle with failed pending-row write");

    assert_eq!(result.failed_count, 1);
    assert_eq!(result.stats.state_write_failures, 1);
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
}

#[tokio::test]
async fn run_cycle_durable_retry_survives_checkpoint_commit_failure() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(1)
        .mount(&server)
        .await;

    let config = make_run_cycle_config();
    let inner = make_state_db();
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        Arc::clone(&inner),
        MetadataSetFailure::Prefix(SYNC_TOKEN_PREFIX),
        "simulated checkpoint commit failure",
    ));
    let body = b"expired-body";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let album = make_one_photo_incremental_album_with_download(
        "PrimarySync",
        "zone-tok-next",
        &format!("{}/expired.jpg", server.uri()),
        body.len() as u64,
        &checksum,
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
        false,
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
}

#[tokio::test]
async fn run_cycle_failed_token_repair_preserves_prior_sqlite_checkpoint() {
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .expect("file-backed state"),
    );
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
        make_empty_full_album(""),
    );
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
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
    let db = make_state_db();
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
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(LegacyPendingDeleteSession {
            master_record_name: Arc::from(master_record_name),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let download_dir = tempfile::tempdir().expect("download tempdir");
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
        false,
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
}

#[tokio::test]
async fn run_cycle_multi_zone_status_preserves_an_earlier_checkpoint_hold() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let held = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_empty_full_album(""),
    );
    let advanced = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        make_empty_full_album("shared-token"),
    );
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&held, &advanced],
        &config,
        Some(db.as_ref()),
        false,
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
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().expect("state directory");
    let inner: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .expect("file-backed state"),
    );
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let shutdown_token = CancellationToken::new();
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(Arc::clone(&inner), "unused")
            .with_cancel_on_upsert(shutdown_token.clone()),
    );
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_one_photo_incremental_album_for_zone("PrimarySync", "zone-tok-new"),
    );
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &shutdown_token,
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
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
}

#[tokio::test]
async fn checkpoint_pilot_holds_preserve_file_backed_state_across_cycles() {
    for hold in ["dry_run", "missing_token", "stale_plan"] {
        let mut config = make_run_cycle_config();
        config.runtime.dry_run = hold == "dry_run";
        let state_dir = tempfile::tempdir().unwrap();
        let db: Arc<dyn download::DownloadStore> = Arc::new(
            state::SqliteStateDb::open(&state_dir.path().join("state.db"))
                .await
                .unwrap(),
        );
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
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));
        let controls = if config.runtime.dry_run {
            download::DownloadControls::dry_run_hidden()
        } else {
            download::DownloadControls::download_hidden()
        };
        for _ in 0..2 {
            let mut library = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                make_empty_full_album(if hold == "missing_token" {
                    ""
                } else {
                    "candidate-token"
                }),
            );
            library.plan_is_stale = hold == "stale_plan";
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                false,
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
async fn full_checkpoint_pilot_advances_with_durable_transfer_failure() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/expired.jpg"))
        .respond_with(ResponseTemplate::new(410))
        .expect(1)
        .mount(&server)
        .await;
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().unwrap();
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        state::SqliteStateDb::open(&state_dir.path().join("state.db"))
            .await
            .unwrap(),
    );
    let download_dir = tempfile::tempdir().unwrap();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let body = b"expired-body";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(full_album_page_with_download(
                "PrimarySync",
                "master-full-expired",
                "full-token",
                &format!("{}/expired.jpg", server.uri()),
                body.len() as u64,
                &checksum,
            )),
    );
    let library =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let builder = make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
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
        Some("full-token")
    );
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending + summary.failed, 1);
    assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
}
