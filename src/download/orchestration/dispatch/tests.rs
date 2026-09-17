use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotosSession, SyncTokenError};
use crate::state::SqliteStateDb;
use crate::test_helpers::{MockPhotosFlow, mock_photo_query_page};

use super::super::config::DownloadConfig;
use super::super::models::{
    DownloadControls, DownloadOutcome, DownloadReporting, DownloadRunMode, DownloadStore,
    FullEnumerationReason, SMART_FOLDER_REFRESH_FAILED_REASON, SyncMode, SyncResult,
};
use super::super::test_support::{
    changes_album, changes_album_with_container, changes_zone_session, incremental_photo_records,
    incremental_test_config, mock_album, seed_complete_album_snapshot, test_config,
    unused_unfiled_changes_pass,
};
use super::{IncrementalErrorClass, classify_incremental_error, download_photos_with_sync};

fn reqwest_status_error(status: u16) -> anyhow::Error {
    let response = http::Response::builder()
        .status(status)
        .body(Vec::<u8>::new())
        .expect("response");
    reqwest::Response::from(response)
        .error_for_status()
        .expect_err("status should be an error")
        .into()
}

fn classify_incremental_error_for(error: SyncTokenError) -> IncrementalErrorClass {
    let error = anyhow::Error::new(error);
    classify_incremental_error(&error)
}

#[test]
fn classify_incremental_error_detects_token_fallback_errors() {
    assert_eq!(
        classify_incremental_error_for(SyncTokenError::InvalidToken {
            reason: "expired".into(),
        }),
        IncrementalErrorClass::TokenFallback
    );
    assert_eq!(
        classify_incremental_error_for(SyncTokenError::ZoneNotFound {
            zone_name: "PrimarySync".into(),
        }),
        IncrementalErrorClass::TokenFallback
    );
}

#[test]
fn classify_incremental_error_detects_transient_errors() {
    let auth_error: anyhow::Error = crate::auth::error::AuthError::ApiError {
        code: 503,
        message: "unavailable".into(),
    }
    .into();
    assert_eq!(
        classify_incremental_error(&auth_error),
        IncrementalErrorClass::TransientFailure
    );

    let reqwest_429 = reqwest_status_error(429);
    assert_eq!(
        classify_incremental_error(&reqwest_429),
        IncrementalErrorClass::TransientFailure
    );

    let reqwest_503 = reqwest_status_error(503);
    assert_eq!(
        classify_incremental_error(&reqwest_503),
        IncrementalErrorClass::TransientFailure
    );
}

#[test]
fn classify_incremental_error_treats_static_and_generic_errors_as_fallback() {
    assert_eq!(
        classify_incremental_error_for(SyncTokenError::UnexpectedZoneError {
            zone_name: "PrimarySync".into(),
            error_code: "TRY_AGAIN_LATER".into(),
        }),
        IncrementalErrorClass::StaticFallback
    );

    let reqwest_400 = reqwest_status_error(400);
    assert_eq!(
        classify_incremental_error(&reqwest_400),
        IncrementalErrorClass::StaticFallback
    );

    let generic = anyhow::anyhow!("decode failed");
    assert_eq!(
        classify_incremental_error(&generic),
        IncrementalErrorClass::StaticFallback
    );
}

#[test]
fn classify_incremental_error_detects_context_wrapped_errors() {
    let token = anyhow::Error::new(SyncTokenError::InvalidToken {
        reason: "expired".into(),
    })
    .context("changes/zone");
    assert_eq!(
        classify_incremental_error(&token),
        IncrementalErrorClass::TokenFallback
    );

    let transient = reqwest_status_error(503).context("changes/zone");
    assert_eq!(
        classify_incremental_error(&transient),
        IncrementalErrorClass::TransientFailure
    );
}

#[derive(Clone)]
struct CountingQuerySession {
    count_query_calls: Arc<AtomicUsize>,
    records_query_calls: Arc<AtomicUsize>,
    page: Value,
    asset_count: u64,
    fail_records_query: bool,
}

impl CountingQuerySession {
    fn new(page: Value, asset_count: u64) -> Self {
        Self {
            count_query_calls: Arc::new(AtomicUsize::new(0)),
            records_query_calls: Arc::new(AtomicUsize::new(0)),
            page,
            asset_count,
            fail_records_query: false,
        }
    }

    fn failing(page: Value, asset_count: u64) -> Self {
        Self {
            fail_records_query: true,
            ..Self::new(page, asset_count)
        }
    }

    fn count_query_count(&self) -> usize {
        self.count_query_calls.load(Ordering::SeqCst)
    }

    fn records_query_count(&self) -> usize {
        self.records_query_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl PhotosSession for CountingQuerySession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            self.count_query_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": self.asset_count}}}]}]
            }));
        }

        if url.contains("/records/query?") {
            let call = self.records_query_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_records_query {
                anyhow::bail!("smart folder refresh failed");
            }
            if call == 0 {
                return Ok(self.page.clone());
            }
            return Ok(json!({
                "records": [],
                "syncToken": self.page.get("syncToken").cloned().unwrap_or(Value::Null)
            }));
        }

        Ok(json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct BackfillAndChangesSession {
    changes_zone_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for BackfillAndChangesSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 0}}}]}]
            }));
        }

        if url.contains("/changes/zone?") {
            self.changes_zone_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-token-next",
                    "moreComing": false,
                    "records": [],
                }]
            }));
        }

        Ok(json!({"records": [], "syncToken": "album-token"}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn smart_folder_unfiled_passes(
    changes_calls: Arc<AtomicUsize>,
    unfiled_records: Vec<Value>,
    smart_session: CountingQuerySession,
) -> Vec<AlbumPass> {
    vec![
        AlbumPass {
            kind: PassKind::SmartFolder,
            album: changes_album("Favorites", smart_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", changes_zone_session(changes_calls, unfiled_records)),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ]
}

async fn run_print_incremental_sync(passes: &[AlbumPass], config: DownloadConfig) -> SyncResult {
    download_photos_with_sync(
        &Client::new(),
        passes,
        Arc::new(config),
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("print-only incremental sync should succeed")
}

#[tokio::test]
async fn unfiled_only_incremental_ignores_inactive_album_path_templates() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        incremental_photo_records("MASTER_CHANGED"),
    );
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    assert!(
        config.requires_per_pass_paths(),
        "default inactive album templates still contain per-pass tokens"
    );
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("unfiled-only incremental sync should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert!(
        !result.full_enumeration_ran,
        "inactive album/smart-folder templates must not force full enumeration"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "unfiled-only incremental sync should query changes/zone"
    );
}

#[tokio::test]
async fn smart_folder_incremental_with_unfiled_refreshes_smart_without_library_all() {
    let changes_calls = Arc::new(AtomicUsize::new(0));
    let smart_session = CountingQuerySession::new(
        mock_photo_query_page("SMART_CHANGED", Some("zone-token")),
        1,
    );
    let passes = smart_folder_unfiled_passes(
        Arc::clone(&changes_calls),
        incremental_photo_records("MASTER_CHANGED"),
        smart_session.clone(),
    );
    let dir = TempDir::new().expect("temp dir");
    let config = incremental_test_config(&dir);

    let result = run_print_incremental_sync(&passes, config).await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert!(
        result.full_enumeration_ran,
        "the selected smart-folder stream still refreshes by records/query"
    );
    assert_eq!(
        changes_calls.load(Ordering::SeqCst),
        1,
        "unfiled follow-up work should use changes/zone"
    );
    assert_eq!(
        smart_session.count_query_count(),
        1,
        "smart-folder refresh should probe the selected smart folder"
    );
    assert_eq!(
        smart_session.records_query_count(),
        1 + crate::icloud::photos::MAX_EMPTY_PAGE_PROBES as usize,
        "smart-folder refresh should enumerate the selected smart folder"
    );
    assert!(
        !result.stats.sync_token_blocked,
        "successful smart-folder refresh should not block an otherwise safe incremental cycle"
    );
}

#[tokio::test]
async fn smart_folder_refresh_blank_query_token_does_not_block_incremental_zone_token() {
    let changes_calls = Arc::new(AtomicUsize::new(0));
    let smart_session = CountingQuerySession::new(
        json!({
            "records": [],
            "syncToken": ""
        }),
        0,
    );
    let passes = smart_folder_unfiled_passes(
        Arc::clone(&changes_calls),
        Vec::new(),
        smart_session.clone(),
    );
    let dir = TempDir::new().expect("temp dir");
    let config = incremental_test_config(&dir);

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("incremental sync plus smart-folder refresh should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        changes_calls.load(Ordering::SeqCst),
        1,
        "unfiled incremental changes should still be checked"
    );
    assert_eq!(
        smart_session.records_query_count(),
        crate::icloud::photos::MAX_EMPTY_PAGE_PROBES as usize
    );
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(
        !result.stats.sync_token_blocked,
        "blank smart-folder query token is telemetry for the refresh, not a reason to block the incremental zone token"
    );
}

#[tokio::test]
async fn smart_folder_incremental_recent_global_does_not_build_library_frontier() {
    let changes_calls = Arc::new(AtomicUsize::new(0));
    let smart_session = CountingQuerySession::new(
        mock_photo_query_page("SMART_CHANGED", Some("zone-token")),
        1,
    );
    let passes = smart_folder_unfiled_passes(
        Arc::clone(&changes_calls),
        Vec::new(),
        smart_session.clone(),
    );
    let dir = TempDir::new().expect("temp dir");
    let mut config = incremental_test_config(&dir);
    config.recent = Some(1);
    config.recent_scope = crate::cli::RecentScope::Global;

    let result = run_print_incremental_sync(&passes, config).await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(changes_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        smart_session.records_query_count(),
        1 + crate::icloud::photos::MAX_EMPTY_PAGE_PROBES as usize,
        "smart-folder refresh must enumerate only the selected smart-folder stream"
    );
}

#[tokio::test]
async fn smart_folder_refresh_failure_blocks_incremental_token() {
    let changes_calls = Arc::new(AtomicUsize::new(0));
    let smart_session = CountingQuerySession::failing(
        mock_photo_query_page("SMART_CHANGED", Some("zone-token")),
        1,
    );
    let passes = smart_folder_unfiled_passes(
        Arc::clone(&changes_calls),
        Vec::new(),
        smart_session.clone(),
    );
    let dir = TempDir::new().expect("temp dir");
    let config = incremental_test_config(&dir);

    let result = run_print_incremental_sync(&passes, config).await;

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(
        changes_calls.load(Ordering::SeqCst),
        1,
        "unfiled incremental changes should still be checked"
    );
    assert_eq!(smart_session.records_query_count(), 1);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(SMART_FOLDER_REFRESH_FAILED_REASON)
    );
    assert_eq!(result.sync_token, None);
}

#[tokio::test]
async fn album_incremental_records_relation_hydration_full_enumeration_reason() {
    let session = MockPhotosFlow::new()
        .album_count(0)
        .empty_query_page(Some("zone-token-next"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Vacation", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("album incremental should fall back to full enumeration");

    assert!(result.full_enumeration_ran);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(FullEnumerationReason::AlbumRelationHydrationIncomplete)
    );
}

#[tokio::test]
async fn album_incremental_with_complete_snapshot_uses_changes_zone() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(Arc::clone(&calls), Vec::new());
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container("Vacation", Some("container-vacation"), session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        unused_unfiled_changes_pass(),
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("trusted album snapshot should allow incremental sync");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert!(
        !result.full_enumeration_ran,
        "complete album snapshots should avoid whole-library fallback"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "trusted album incremental sync should query changes/zone once"
    );
}

#[tokio::test]
async fn album_incremental_missing_snapshot_runs_targeted_backfill() {
    let changes_zone_calls = Arc::new(AtomicUsize::new(0));
    let album_session = BackfillAndChangesSession {
        changes_zone_calls: Arc::clone(&changes_zone_calls),
    };
    let unfiled_session =
        CountingQuerySession::new(json!({"records": [], "syncToken": "unfiled-token"}), 0);
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container(
                "Vacation",
                Some("container-vacation"),
                album_session,
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", unfiled_session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("missing snapshot should run targeted backfill");

    assert!(result.full_enumeration_ran);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(FullEnumerationReason::AlbumRelationHydrationIncomplete)
    );
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert_eq!(
        changes_zone_calls.load(Ordering::SeqCst),
        1,
        "targeted backfill should still drain the zone delta once"
    );
    assert_eq!(
        unfiled_session.records_query_count(),
        0,
        "targeted backfill must not enumerate the library-wide unfiled pass"
    );
    assert!(
        db.selected_album_containers_have_complete_snapshots(
            "PrimarySync",
            &["container-vacation"],
        )
        .await
        .unwrap(),
        "targeted backfill should complete the missing album snapshot"
    );
}

#[tokio::test]
async fn targeted_album_backfill_failure_blocks_incremental_token() {
    let album_session =
        CountingQuerySession::failing(json!({"records": [], "syncToken": "album-token"}), 1);
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: changes_album_with_container("Vacation", Some("container-vacation"), album_session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("targeted backfill failure should return a token-blocked result");

    assert_eq!(result.sync_token, None);
    assert!(result.stats.sync_token_blocked);
}
