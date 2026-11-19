use std::path::Path;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use futures_util::stream;
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::download::pipeline::{StreamRuntime, stream_and_download_from_stream};
use crate::download::retry::{
    PendingRetryTarget, build_pending_retry_download_tasks, take_matching_pending_retry_tasks,
};
use crate::download::{file, filter};
use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::retry::RetryConfig;
use crate::state::VersionSizeKey;
use crate::test_helpers::{
    MockPhotosFlow, TestAssetRecord, mock_photo_records_for_zone_with_filename,
};
use crate::types::FileMatchPolicy;

use super::super::dispatch::download_photos_with_sync;
use super::super::models::{
    DownloadControls, DownloadOutcome, DownloadReporting, DownloadRunMode, SyncMode, SyncResult,
};
use super::super::test_support::{
    PendingLookupSession, SelectedMediaSession, album_with_session, changes_zone_response,
    hard_deleted_change_record, incremental_photo_records_with_url, initialize_empty_selection,
    mock_album, mock_asset_record_for, mock_master_record_with_filename,
    mock_photo_records_with_filename, open_fixture_db, retry_test_task, selected_pass_with_session,
    test_config,
};

#[test]
fn pending_retry_filter_matches_asset_version_without_path() {
    let original = retry_test_task("ASSET_A", VersionSizeKey::Original, "old/a.jpg");
    let refreshed_elsewhere =
        retry_test_task("ASSET_A", VersionSizeKey::Original, "new/location/a.jpg");
    let wrong_version = retry_test_task("ASSET_A", VersionSizeKey::Medium, "new/a.jpg");
    let unrelated = retry_test_task("ASSET_B", VersionSizeKey::Original, "new/b.jpg");
    let mut pending_targets: FxHashSet<PendingRetryTarget> = std::iter::once(PendingRetryTarget {
        library: Arc::from("PrimarySync"),
        asset_id: Arc::clone(&original.asset_id),
        version_size: original.version_size,
    })
    .collect();
    let mut out = Vec::new();

    take_matching_pending_retry_tasks(
        vec![wrong_version, unrelated, refreshed_elsewhere.clone()],
        &mut pending_targets,
        &mut out,
    );

    assert!(pending_targets.is_empty());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].asset_id.as_ref(), "ASSET_A");
    assert_eq!(out[0].version_size, VersionSizeKey::Original);
    assert_eq!(out[0].download_path, refreshed_elsewhere.download_path);
}

#[derive(Clone, Debug)]
struct LegacyPendingHydrationSession {
    lookup_records: Arc<Vec<Value>>,
    hydration_records: Arc<Vec<Value>>,
    hydration_error: Option<Arc<str>>,
}

#[async_trait::async_trait]
impl PhotosSession for LegacyPendingHydrationSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            return Ok(json!({"records": self.lookup_records.as_ref().clone()}));
        }
        if url.contains("/changes/zone?") {
            let request: Value = serde_json::from_str(&body)?;
            let has_sync_token = request["zones"]
                .as_array()
                .and_then(|zones| zones.first())
                .and_then(|zone| zone.get("syncToken"))
                .is_some();
            let records = if has_sync_token {
                Vec::new()
            } else {
                if let Some(error) = &self.hydration_error {
                    anyhow::bail!(error.to_string());
                }
                self.hydration_records.as_ref().clone()
            };
            return Ok(changes_zone_response(records, "zone-token-next"));
        }
        Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn incremental_with_failed_rows_uses_targeted_retry_not_full_enumeration() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = crate::test_helpers::TestAssetRecord::new("FAILED_BEFORE_SYNC")
        .filename("failed-before-sync.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.mark_failed(
        "PrimarySync",
        "FAILED_BEFORE_SYNC",
        "original",
        "prior download failure",
    )
    .await
    .expect("mark failed");
    db.upsert_asset_master_mapping(
        "PrimarySync",
        "asset-FAILED_BEFORE_SYNC",
        "FAILED_BEFORE_SYNC",
    )
    .await
    .expect("seed asset/master mapping");

    let session = PendingLookupSession {
        records: Arc::new(mock_photo_records_for_zone_with_filename(
            "FAILED_BEFORE_SYNC",
            "PrimarySync",
            "failed-before-sync.jpg",
        )),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
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
    .expect("failed rows should use targeted retry");

    assert!(
        !result.full_enumeration_ran,
        "normal sync with failed rows should not force full enumeration"
    );
    assert_eq!(result.stats.full_enumeration_reason, None);
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "result: {result:?}"
    );
    assert_eq!(
        result.sync_token, None,
        "print-only targeted retry must not advance the zone token"
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn pending_peer_progresses_during_partial_failure_without_extra_same_cycle_attempts() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Clone, Debug)]
    struct Session {
        delta: Vec<Value>,
        records: Vec<Value>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for Session {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                return Ok(changes_zone_response(self.delta.clone(), "next"));
            }
            assert!(
                url.contains("/records/lookup?"),
                "recovery must not enumerate"
            );
            Ok(json!({"records": self.records}))
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let server = MockServer::start().await;
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/pattern.jpg"
    ));
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let records_for = |id: &str| {
        let mut records = incremental_photo_records_with_url(
            id,
            &format!("{id}.jpg"),
            &format!("{}/{id}", server.uri()),
            bytes.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
            json!("_defaultOwner");
        records
    };
    let peer = records_for("peer");
    let failing = records_for("failing");
    let root = TempDir::new().unwrap();
    let database = root.path().join("state.db");
    let mut config = test_config();
    config.directory = Arc::from(root.path().join("media"));
    config.retry.max_retries = 0;
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "before".into(),
    };
    {
        let db = open_fixture_db(&database).await;
        let asset = PhotoAsset::new(peer[0].clone(), peer[1].clone());
        let destination = filter::expected_paths_for(&asset, &config).remove(0).path;
        seed_pending_retry_with_recorded_path(
            &db,
            &asset,
            &checksum,
            bytes.len() as u64,
            &destination,
        )
        .await;
        // Admit both selected destinations through a failed production cycle.
        // Historical aggregate rows alone must not manufacture retry work.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let passes = [selected_pass_with_session(
            &db,
            "",
            Box::new(SelectedMediaSession(Arc::new(
                [peer.clone(), failing.clone()].concat(),
            ))),
        )];
        let mut initial = config.clone();
        initial.state_db = Some(db.clone());
        initial.sync_mode = SyncMode::Full;
        let failed = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(initial),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(failed.outcome, DownloadOutcome::PartialFailure { .. }),
            "{failed:?}"
        );
        assert_eq!(failed.stats.downloaded, 0);
        assert_eq!(failed.sync_token, None);
        assert_eq!(db.get_failed().await.unwrap().len(), 2);
        // Retain the original pending-peer/failed-peer guard matrix.
        db.prepare_for_retry(
            Some("PrimarySync"),
            crate::state::RetryErrorRetention::Clear,
        )
        .await
        .unwrap();
        db.mark_failed(
            "PrimarySync",
            "asset-failing",
            "original",
            "prior selected transfer failed",
        )
        .await
        .unwrap();
        assert_eq!(db.get_pending().await.unwrap().len(), 1);
        server.reset().await;
    }
    Mock::given(method("GET"))
        .and(path("/peer"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/failing"))
        .respond_with(ResponseTemplate::new(503))
        .expect(2)
        .mount(&server)
        .await;
    for round in 0..4 {
        let db = open_fixture_db(&database).await;
        config.state_db = Some(db.clone());
        let passes = [selected_pass_with_session(
            &db,
            "",
            Box::new(Session {
                delta: Vec::new(),
                records: [peer.clone(), failing.clone()].concat(),
            }),
        )];
        if round == 0 {
            for (outcome, state_failures, interrupted) in [
                (
                    DownloadOutcome::SessionExpired {
                        auth_error_count: 1,
                    },
                    0,
                    false,
                ),
                (
                    DownloadOutcome::PartialFailure { failed_count: 1 },
                    1,
                    false,
                ),
                (DownloadOutcome::PartialFailure { failed_count: 1 }, 0, true),
            ] {
                let mut blocked = SyncResult::from_execution(outcome, None, Default::default());
                blocked.retry_exclusions.insert(PendingRetryTarget {
                    library: "PrimarySync".into(),
                    asset_id: "asset-failing".into(),
                    version_size: VersionSizeKey::Original,
                });
                blocked.checkpoint.state_write_failures = state_failures;
                blocked.checkpoint.interrupted = interrupted;
                let result = super::append_targeted_recovery_to_sync_result(
                    &Client::new(),
                    &passes,
                    &Arc::new(config.clone()),
                    DownloadControls::download_hidden(),
                    CancellationToken::new(),
                    blocked,
                )
                .await
                .unwrap();
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(db.get_pending().await.unwrap().len(), 1);
                assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
                assert!(server.received_requests().await.unwrap().is_empty());
            }
            for guard in 0..6 {
                let mut blocked = SyncResult::from_execution(
                    DownloadOutcome::PartialFailure { failed_count: 1 },
                    None,
                    Default::default(),
                );
                let mut source =
                    SyncResult::from_execution(DownloadOutcome::Success, None, Default::default());
                source.retry_exclusions.insert(PendingRetryTarget {
                    library: "PrimarySync".into(),
                    asset_id: "asset-failing".into(),
                    version_size: VersionSizeKey::Original,
                });
                blocked.accumulate(&source);
                assert_eq!(blocked.retry_exclusions, source.retry_exclusions);
                match guard {
                    0 => blocked.checkpoint.sync_token_blocked = true,
                    1 => blocked.checkpoint.identity_incomplete = true,
                    2 => blocked.checkpoint.enumeration_incomplete = true,
                    3 => blocked.checkpoint.enumeration_errors = 1,
                    4 => blocked.retry_exclusions.clear(),
                    _ => blocked.outcome = DownloadOutcome::PartialFailure { failed_count: 2 },
                }
                let result = super::append_targeted_recovery_to_sync_result(
                    &Client::new(),
                    &passes,
                    &Arc::new(config.clone()),
                    DownloadControls::download_hidden(),
                    CancellationToken::new(),
                    blocked,
                )
                .await
                .unwrap();
                assert_eq!(result.stats.downloaded, 0, "guard {guard}");
                assert_eq!(db.get_pending().await.unwrap().len(), 1);
                assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
                assert!(server.received_requests().await.unwrap().is_empty());
            }
        }
        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.stats.downloaded,
            usize::from(round < 2),
            "{result:?}"
        );
        assert!(!result.full_enumeration_ran);
        let completed = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(completed.len(), if round == 0 { 1 } else { 2 });
        for record in completed {
            assert_eq!(std::fs::read(record.local_path.unwrap()).unwrap(), bytes);
        }
        if round == 0 {
            assert!(matches!(
                result.outcome,
                DownloadOutcome::PartialFailure { failed_count: 1 }
            ));
            let failed = db.get_failed().await.unwrap();
            assert_eq!(failed.len(), 1);
            assert!(
                failed
                    .iter()
                    .any(|record| record.id.as_ref() == "asset-failing")
            );
            server.verify().await;
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/failing"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/peer"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .expect(0)
                .mount(&server)
                .await;
        } else {
            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "{result:?}"
            );
            assert!(db.get_failed().await.unwrap().is_empty());
        }
        assert!(db.get_pending().await.unwrap().is_empty());
        config.state_db = None;
    }
    server.verify().await;
}

async fn seed_pending_retry_with_recorded_path(
    db: &crate::state::SqliteStateDb,
    asset: &PhotoAsset,
    checksum: &str,
    size: u64,
    recorded_path: &Path,
) {
    let filename = recorded_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("recorded filename");
    let record = TestAssetRecord::new(asset.state_id())
        .filename(filename)
        .checksum(checksum)
        .size(size)
        .added_at(asset.added_date())
        .build();
    db.upsert_seen(&record).await.expect("seed state row");
    db.mark_downloaded(
        "PrimarySync",
        asset.state_id(),
        VersionSizeKey::Original.as_str(),
        recorded_path,
        "previous-local-checksum",
        None,
    )
    .await
    .expect("seed recorded path");
    db.mark_failed(
        "PrimarySync",
        asset.state_id(),
        VersionSizeKey::Original.as_str(),
        "retry recorded path",
    )
    .await
    .expect("mark retry pending");
    db.prepare_for_retry(
        Some("PrimarySync"),
        crate::state::RetryErrorRetention::Clear,
    )
    .await
    .expect("prepare failed row for retry");
    db.upsert_asset_master_mapping("PrimarySync", asset.asset_record_name(), asset.state_id())
        .await
        .expect("seed asset/master mapping");
}

#[tokio::test]
async fn native_retry_identity_requires_exact_recorded_child_ownership() {
    use super::super::context::{ClaimedLegacyMasterStates, DownloadContext};
    for evidence in ["mapped", "unmapped", "multiple", "protected"] {
        let root = TempDir::new().unwrap();
        let database = root.path().join("state.db");
        let db = open_fixture_db(&database).await;
        let records = incremental_photo_records_with_url(
            "RETRY",
            "retry.jpg",
            "https://p01.icloud-content.com/retry.jpg",
            8,
        );
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let checksum = records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"]
            .as_str()
            .unwrap();
        db.upsert_seen(
            &TestAssetRecord::new("RETRY")
                .filename("retry.jpg")
                .checksum(checksum)
                .size(8)
                .added_at(asset.added_date())
                .build(),
        )
        .await
        .unwrap();
        if evidence != "unmapped" {
            db.upsert_asset_master_mapping("PrimarySync", "asset-RETRY", "RETRY")
                .await
                .unwrap();
        }
        if evidence == "multiple" {
            db.upsert_asset_master_mapping("PrimarySync", "other-child", "RETRY")
                .await
                .unwrap();
        }
        if evidence == "protected" {
            db.acquire_lock("protected retry history").unwrap().execute_batch(
                "INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at) VALUES('PrimarySync','RETRY',1,'{}','[]',1)"
            ).unwrap();
        }
        drop(db);
        for _ in 0..2 {
            let db = open_fixture_db(&database).await;
            let context = DownloadContext::load(db.as_ref()).await;
            assert!(
                context.has_matching_state_checksum("PrimarySync", "RETRY", &asset),
                "fixture must retain a parsed current resource matching the pending row"
            );
            let selected = context
                .select_current_asset_state_record_name_for_download(
                    Some(db.as_ref()),
                    "PrimarySync",
                    &asset,
                    &mut ClaimedLegacyMasterStates::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                selected.as_ref(),
                if evidence == "mapped" {
                    "RETRY"
                } else {
                    "asset-RETRY"
                },
                "{evidence}"
            );
            let owners: i64 = db
                .acquire_lock("only proven retry ownership is durable")
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM legacy_master_state_owners",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(owners, i64::from(evidence == "mapped"), "{evidence}");
        }
    }
}

#[tokio::test]
async fn pending_retry_reuses_missing_recorded_path_when_it_matches_planned_path() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());

    let mut records = incremental_photo_records_with_url(
        "MISSING_PATH",
        "missing.jpg",
        "https://p01.icloud-content.com/missing.jpg",
        8,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("ck_missing");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let recorded_path = filter::expected_paths_for(&asset, &config)
        .into_iter()
        .next()
        .expect("asset should derive a path")
        .path;
    seed_pending_retry_with_recorded_path(db.as_ref(), &asset, "ck_missing", 8, &recorded_path)
        .await;

    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let plan = build_pending_retry_download_tasks(
        &passes,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .expect("plan targeted retry");

    assert_eq!(plan.tasks.len(), 1);
    assert_eq!(plan.tasks[0].download_path, recorded_path);
}

#[tokio::test]
async fn name_id7_targeted_retry_adopts_intact_recorded_path_without_planned_task() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.file_match_policy = FileMatchPolicy::NameId7;

    let mut records = incremental_photo_records_with_url(
        "ID7_ADOPT",
        "id7.jpg",
        "https://p01.icloud-content.com/id7.jpg",
        8,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("ck_id7");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let recorded_path = filter::expected_paths_for(&asset, &config)
        .into_iter()
        .next()
        .expect("asset should derive a path")
        .path;
    tokio::fs::create_dir_all(recorded_path.parent().expect("recorded path parent"))
        .await
        .expect("create recorded path parent");
    tokio::fs::write(&recorded_path, b"12345678")
        .await
        .expect("seed intact recorded file");
    seed_pending_retry_with_recorded_path(db.as_ref(), &asset, "ck_id7", 8, &recorded_path).await;
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let plan = build_pending_retry_download_tasks(
        &passes,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .expect("adopt targeted retry");

    assert!(plan.tasks.is_empty());
    let summary = db.get_summary().await.expect("state summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.policy_excluded, 0);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(
        downloaded[0].local_path.as_deref(),
        Some(recorded_path.as_path())
    );
}

#[tokio::test]
async fn targeted_retry_adopts_smaller_metadata_rewritten_recorded_file() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());

    let mut records = incremental_photo_records_with_url(
        "METADATA_REWRITTEN_RETRY",
        "rewritten.jpg",
        "https://p01.icloud-content.com/rewritten.jpg",
        8,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!("ck_metadata_rewritten_retry");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let recorded_path = filter::expected_paths_for(&asset, &config)
        .into_iter()
        .next()
        .expect("asset should derive a path")
        .path;
    tokio::fs::create_dir_all(recorded_path.parent().expect("recorded path parent"))
        .await
        .expect("create recorded path parent");
    tokio::fs::write(&recorded_path, b"shorter")
        .await
        .expect("seed metadata-rewritten file");
    let local_checksum = file::compute_sha256(&recorded_path)
        .await
        .expect("hash metadata-rewritten file");

    let record = TestAssetRecord::new(asset.state_id())
        .filename("rewritten.jpg")
        .checksum("ck_metadata_rewritten_retry")
        .size(8)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed state row");
    db.mark_downloaded(
        "PrimarySync",
        asset.state_id(),
        VersionSizeKey::Original.as_str(),
        &recorded_path,
        &local_checksum,
        Some("download-checksum-before-metadata"),
    )
    .await
    .expect("seed metadata-rewritten state");
    db.mark_failed(
        "PrimarySync",
        asset.state_id(),
        VersionSizeKey::Original.as_str(),
        "prior false truncation",
    )
    .await
    .expect("mark row failed");
    db.prepare_for_retry(
        Some("PrimarySync"),
        crate::state::RetryErrorRetention::Clear,
    )
    .await
    .expect("prepare row for retry");
    db.upsert_asset_master_mapping("PrimarySync", asset.asset_record_name(), asset.state_id())
        .await
        .expect("seed asset/master mapping");

    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let plan = build_pending_retry_download_tasks(
        &passes,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .expect("adopt metadata-rewritten retry");

    assert!(plan.tasks.is_empty());
    assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), b"shorter");
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 1);
    assert_eq!(
        downloaded[0].local_path.as_deref(),
        Some(recorded_path.as_path())
    );
    assert_eq!(
        downloaded[0].local_checksum.as_deref(),
        Some(local_checksum.as_str())
    );
    assert_eq!(
        downloaded[0].download_checksum.as_deref(),
        Some("download-checksum-before-metadata")
    );
}

#[tokio::test]
async fn approved_truncated_reconcile_retry_replaces_the_recorded_path() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().expect("temp dir");
    let db = open_fixture_db(&dir.path().join("state.db")).await;
    let recorded_path = dir.path().join("2019/11/2019-11-28/photo.jpg");
    tokio::fs::create_dir_all(recorded_path.parent().expect("recorded parent"))
        .await
        .expect("create recorded directory");
    tokio::fs::write(&recorded_path, &body)
        .await
        .expect("seed intact download");
    let original_local_checksum = file::compute_sha256(&recorded_path)
        .await
        .expect("hash intact download");
    let record = crate::test_helpers::TestAssetRecord::new("FAILED_WITH_PATH")
        .filename("photo.jpg")
        .checksum(&checksum)
        .size(body.len() as u64)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.mark_downloaded(
        "PrimarySync",
        "FAILED_WITH_PATH",
        "original",
        &recorded_path,
        &original_local_checksum,
        Some(&original_local_checksum),
    )
    .await
    .expect("seed recorded local path");
    tokio::fs::write(&recorded_path, b"bad")
        .await
        .expect("truncate recorded download");

    let (counts, drift) = crate::commands::reconcile::scan_local_drift(
        db.as_ref(),
        |_: &crate::commands::reconcile::LocalDriftAsset| {},
        |_: &str| {},
    )
    .await
    .expect("scan truncated download");
    assert_eq!(counts.damaged, 1);
    assert_eq!(drift.len(), 1);
    for update in &drift {
        db.mark_failed(
            &update.library,
            &update.id,
            update.version_size.as_str(),
            update.kind.reason(),
        )
        .await
        .expect("mark truncated download failed");
    }
    db.upsert_asset_master_mapping("PrimarySync", "asset-FAILED_WITH_PATH", "FAILED_WITH_PATH")
        .await
        .expect("seed asset/master mapping");

    let download_url = format!("{}/photo.jpg", server.uri());
    let mut records = incremental_photo_records_with_url(
        "FAILED_WITH_PATH",
        "photo.jpg",
        &download_url,
        body.len() as u64,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let passes = vec![selected_pass_with_session(
        &db,
        "AlbumOne",
        Box::new(SelectedMediaSession(Arc::new(records))),
    )];
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.repair_truncated = true;
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
    .expect("retry truncated download through the production pipeline");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), body);
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 1);
    let repaired_path = downloaded[0]
        .local_path
        .as_ref()
        .expect("repair records its final path");
    assert_eq!(repaired_path, &recorded_path);
    let mut directory = tokio::fs::read_dir(recorded_path.parent().unwrap())
        .await
        .expect("read repair directory");
    let mut names = Vec::new();
    while let Some(entry) = directory.next_entry().await.expect("read directory entry") {
        names.push(entry.file_name());
    }
    assert_eq!(names, vec![recorded_path.file_name().unwrap()]);
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn truncated_repair_provider_version_change_uses_sibling() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let current_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let current_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&current_body));
    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(current_body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().expect("temp dir");
    let db = open_fixture_db(&dir.path().join("state.db")).await;
    let recorded_path = dir.path().join("2019/11/2019-11-28/photo.jpg");
    tokio::fs::create_dir_all(recorded_path.parent().expect("recorded parent"))
        .await
        .expect("create recorded directory");
    let prior_body = &current_body[..current_body.len() - 1];
    tokio::fs::write(&recorded_path, prior_body)
        .await
        .expect("seed prior provider version");
    let prior_local_checksum = file::compute_sha256(&recorded_path)
        .await
        .expect("hash prior provider version");
    let record = crate::test_helpers::TestAssetRecord::new("CHANGED_TRUNCATED_PATH")
        .filename("photo.jpg")
        .checksum("old-provider-checksum")
        .size(prior_body.len() as u64)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed state row");
    db.mark_downloaded(
        "PrimarySync",
        "CHANGED_TRUNCATED_PATH",
        "original",
        &recorded_path,
        &prior_local_checksum,
        Some(&prior_local_checksum),
    )
    .await
    .expect("seed recorded local path");
    // The provider bytes change only after this child's ownership is known.
    assert!(
        db.claim_legacy_master_state_owner(
            "PrimarySync",
            "CHANGED_TRUNCATED_PATH",
            "asset-CHANGED_TRUNCATED_PATH"
        )
        .await
        .unwrap()
    );
    tokio::fs::write(&recorded_path, b"bad")
        .await
        .expect("truncate recorded download");

    let (counts, drift) = crate::commands::reconcile::scan_local_drift(
        db.as_ref(),
        |_: &crate::commands::reconcile::LocalDriftAsset| {},
        |_: &str| {},
    )
    .await
    .expect("scan truncated download");
    assert_eq!(counts.damaged, 1);
    assert_eq!(drift.len(), 1);
    for update in &drift {
        db.mark_failed(
            &update.library,
            &update.id,
            update.version_size.as_str(),
            update.kind.reason(),
        )
        .await
        .expect("mark truncated download failed");
    }
    db.upsert_asset_master_mapping(
        "PrimarySync",
        "asset-CHANGED_TRUNCATED_PATH",
        "CHANGED_TRUNCATED_PATH",
    )
    .await
    .expect("seed asset/master mapping");

    let download_url = format!("{}/photo.jpg", server.uri());
    let mut records = incremental_photo_records_with_url(
        "CHANGED_TRUNCATED_PATH",
        "photo.jpg",
        &download_url,
        current_body.len() as u64,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(current_checksum);
    records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let passes = vec![selected_pass_with_session(
        &db,
        "AlbumOne",
        Box::new(SelectedMediaSession(Arc::new(records))),
    )];
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.repair_truncated = true;
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
    .expect("retry changed provider version through production pipeline");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), b"bad");
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 1);
    let current_path = downloaded[0]
        .local_path
        .as_ref()
        .expect("current provider version has a local path");
    assert_ne!(current_path, &recorded_path);
    assert_eq!(tokio::fs::read(current_path).await.unwrap(), current_body);
    assert_eq!(current_path.parent(), recorded_path.parent());
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn same_size_provider_change_retry_does_not_adopt_stale_recorded_path() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let failed_server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/changed.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&failed_server)
        .await;

    let new_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let new_checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&new_body));
    let failed_url = format!("{}/changed.jpg", failed_server.uri());
    let mut changed_records = incremental_photo_records_with_url(
        "SAME_SIZE_CHANGED",
        "changed.jpg",
        &failed_url,
        new_body.len() as u64,
    );
    changed_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(new_checksum);
    let changed_asset = PhotoAsset::new(changed_records[0].clone(), changed_records[1].clone());

    let dir = TempDir::new().expect("temp dir");
    let db = open_fixture_db(&dir.path().join("state.db")).await;
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    config.retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let config = Arc::new(config);

    let recorded_path = filter::expected_paths_for(&changed_asset, config.as_ref())
        .into_iter()
        .next()
        .expect("asset should derive a path")
        .path;
    tokio::fs::create_dir_all(recorded_path.parent().expect("recorded parent"))
        .await
        .expect("create recorded directory");
    let old_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x45];
    tokio::fs::write(&recorded_path, &old_body)
        .await
        .expect("seed old provider version");
    let old_local_checksum = file::compute_sha256(&recorded_path)
        .await
        .expect("hash old provider version");
    let old_record = TestAssetRecord::new("SAME_SIZE_CHANGED")
        .filename("changed.jpg")
        .checksum("old-provider-checksum")
        .size(old_body.len() as u64)
        .build();
    db.upsert_seen(&old_record)
        .await
        .expect("seed old state row");
    db.mark_downloaded(
        "PrimarySync",
        "SAME_SIZE_CHANGED",
        VersionSizeKey::Original.as_str(),
        &recorded_path,
        &old_local_checksum,
        None,
    )
    .await
    .expect("record old provider version");

    let first = stream_and_download_from_stream(
        &Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(changed_asset)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("changed provider version should reach the download pipeline");
    assert_eq!(first.downloaded, 0);
    assert_eq!(first.failed.len(), 1);
    assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), old_body);
    let failed = db.get_failed().await.expect("failed row");
    assert!(failed[0].downloaded_at.is_none());

    let success_server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/changed.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(new_body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&success_server)
        .await;
    changed_records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/changed.jpg", success_server.uri()));
    changed_records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let retry_passes = vec![selected_pass_with_session(
        &db,
        "",
        Box::new(SelectedMediaSession(Arc::new(changed_records))),
    )];

    let retry = download_photos_with_sync(
        &Client::new(),
        &retry_passes,
        config,
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("targeted retry should download the changed provider version");

    assert!(matches!(retry.outcome, DownloadOutcome::Success));
    assert_eq!(retry.stats.downloaded, 1);
    assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), old_body);
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 2);
    let changed_path = downloaded
        .iter()
        .find(|record| record.id.as_ref() == "asset-SAME_SIZE_CHANGED")
        .expect("changed child has a downloaded row")
        .local_path
        .as_ref()
        .expect("changed provider version has a local path");
    assert_ne!(changed_path, &recorded_path);
    assert_eq!(tokio::fs::read(changed_path).await.unwrap(), new_body);
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 2);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn incremental_pending_retry_dry_run_counts_planned_retry_without_token() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = crate::test_helpers::TestAssetRecord::new("DRY_RUN_PENDING")
        .filename("dry-run-pending.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.upsert_asset_master_mapping("PrimarySync", "asset-DRY_RUN_PENDING", "DRY_RUN_PENDING")
        .await
        .expect("seed asset/master mapping");

    let session = PendingLookupSession {
        records: Arc::new(mock_photo_records_for_zone_with_filename(
            "DRY_RUN_PENDING",
            "PrimarySync",
            "dry-run-pending.jpg",
        )),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::new(DownloadRunMode::DryRun, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("dry-run pending retry should report planned work");

    assert!(
        !result.full_enumeration_ran,
        "dry-run pending retry should not force full enumeration"
    );
    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(result.sync_token, None);
    assert!(
        db.get_legacy_master_state_owners()
            .await
            .unwrap()
            .is_empty()
    );

    assert!(!result.stats.sync_token_blocked);
}

#[tokio::test]
async fn incremental_with_failed_rows_retries_real_download_after_zone_delta() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let response_ready = ready.clone();
    let successful_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_successes = successful_requests.clone();
    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    let response_body = body.clone();
    Mock::given(method("GET"))
        .and(path("/failed-before-sync.jpg"))
        .respond_with(move |_: &wiremock::Request| {
            if !response_ready.load(std::sync::atomic::Ordering::SeqCst) {
                return ResponseTemplate::new(404);
            }
            response_successes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200)
                .set_body_bytes(response_body.clone())
                .insert_header("content-type", "image/jpeg")
        })
        .mount(&server)
        .await;

    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    let record = crate::test_helpers::TestAssetRecord::new("FAILED_BEFORE_SYNC")
        .filename("failed-before-sync.jpg")
        .checksum(&checksum)
        .size(8)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.mark_failed(
        "PrimarySync",
        "FAILED_BEFORE_SYNC",
        "original",
        "prior download failure",
    )
    .await
    .expect("mark failed");
    db.upsert_asset_master_mapping(
        "PrimarySync",
        "asset-FAILED_BEFORE_SYNC",
        "FAILED_BEFORE_SYNC",
    )
    .await
    .expect("seed asset/master mapping");

    let download_url = format!("{}/failed-before-sync.jpg", server.uri());
    let mut records = incremental_photo_records_with_url(
        "FAILED_BEFORE_SYNC",
        "failed-before-sync.jpg",
        &download_url,
        8,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
    records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    #[derive(Clone)]
    struct QuietRetrySession(SelectedMediaSession);
    #[async_trait::async_trait]
    impl PhotosSession for QuietRetrySession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                return Ok(changes_zone_response(Vec::new(), "zone-token-next"));
            }
            self.0.post(url, body, headers).await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    let session = QuietRetrySession(SelectedMediaSession(Arc::new(records)));
    let passes = vec![selected_pass_with_session(
        &db,
        "",
        Box::new(session.clone()),
    )];

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    let mut initial = config.clone();
    initial.sync_mode = SyncMode::Full;
    let failed = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(initial),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(failed.outcome, DownloadOutcome::PartialFailure { .. }),
        "{failed:?}"
    );
    assert_eq!(failed.sync_token, None);
    assert_eq!(failed.stats.downloaded, 0);
    assert_eq!(
        successful_requests.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let initial_requests = server.received_requests().await.unwrap().len();
    assert!(
        (1..=2).contains(&initial_requests),
        "bounded failed initial execution: {initial_requests}"
    );
    config.state_db = None;
    drop(passes);
    drop(db);
    let db = open_fixture_db(&database).await;
    config.state_db = Some(db.clone());
    let passes = vec![selected_pass_with_session(&db, "", Box::new(session))];
    ready.store(true, std::sync::atomic::Ordering::SeqCst);

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("failed rows should retry through the real download path");

    assert!(
        !result.full_enumeration_ran,
        "normal sync with failed rows should not force full enumeration"
    );
    assert_eq!(result.stats.full_enumeration_reason, None);
    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    let downloaded = db
        .get_downloaded_page(0, 10)
        .await
        .expect("downloaded page");
    let local_path = downloaded[0]
        .local_path
        .as_ref()
        .expect("downloaded row has a local path");
    assert!(
        tokio::fs::metadata(local_path).await.is_ok(),
        "targeted retry should finalize the downloaded file"
    );
    assert_eq!(std::fs::read(local_path).unwrap(), body);
    assert_eq!(
        successful_requests.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        initial_requests + 1
    );
    let quiet = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(quiet.outcome, DownloadOutcome::Success),
        "{quiet:?}"
    );
    assert!(!quiet.full_enumeration_ran);
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(quiet.sync_token.as_deref(), Some("zone-token-next"));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        initial_requests + 1
    );
    assert_eq!(std::fs::read(local_path).unwrap(), body);
}

#[tokio::test]
async fn incremental_with_unmatched_pending_rows_does_not_block_source_checkpoint() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = crate::test_helpers::TestAssetRecord::new("PENDING_BEFORE_SYNC")
        .filename("pending-before-sync.jpg")
        .checksum("ck_pending_before_sync")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");

    let session = MockPhotosFlow::new()
        .changes_zone_page(Vec::new(), "zone-token-next", false)
        .empty_query_page(Some("ignored-query-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

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
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("pending rows should remain durable without forcing provider recovery");

    assert!(
        !result.full_enumeration_ran,
        "unmatched pending rows should not force full enumeration"
    );
    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.sync_token, None);
    assert_eq!(result.stats.downloaded, 0);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_blocked_reason, None);
}

#[tokio::test]
async fn incremental_with_unmatched_pending_rows_retains_state_without_deletion_evidence() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    let record = TestAssetRecord::new("PENDING_DELETED_UPSTREAM")
        .filename("pending-deleted-upstream.jpg")
        .checksum("ck_pending_deleted_upstream")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.set_asset_verification(
        "PrimarySync",
        "PENDING_DELETED_UPSTREAM",
        "original",
        crate::state::AssetVerificationState::Unknown,
        "prior lookup omitted record",
    )
    .await
    .unwrap();

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    super::super::test_support::initialize_empty_selection(&db, &config).await;
    config.state_db = None;
    drop(db);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    for _ in 0..3 {
        let db = open_fixture_db(&database).await;
        config.state_db = Some(db.clone());
        // The session supplies only a quiet delta. Unexpected historical lookup or
        // enumeration fails the production cycle rather than supplying new facts.
        let session = MockPhotosFlow::new()
            .changes_zone_page(Vec::new(), "zone-token-next", false)
            .build();
        let passes = vec![selected_pass_with_session(&db, "", Box::new(session))];
        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("quiet selection must preserve unresolved historical evidence");

        assert!(
            !result.full_enumeration_ran,
            "retaining unresolved pending state should not force full enumeration"
        );
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_blocked_reason, None);
        assert_eq!(result.stats.stale_pending_pruned, 0);
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.awaiting_provider_verification, 1);
        assert_eq!(db.get_pending().await.expect("pending page").len(), 1);
        assert_eq!(summary.source_deleted, 0);
        assert_eq!(summary.total_assets, 1);
        config.state_db = None;
    }
}

#[tokio::test]
async fn full_enumeration_query_absence_never_deletes_pending_state() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    let record = TestAssetRecord::new("ABSENT_FROM_FULL_QUERY")
        .filename("absent-from-full-query.jpg")
        .checksum("ck_absent_from_full_query")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    // Retain inconclusive historical identity evidence. Current query absence
    // neither resolves it nor authorizes deleting its pending media record.
    db.set_asset_verification(
        "PrimarySync",
        "ABSENT_FROM_FULL_QUERY",
        "original",
        crate::state::AssetVerificationState::Unknown,
        "prior lookup omitted record",
    )
    .await
    .unwrap();

    drop(db);
    for _ in 0..2 {
        let db = open_fixture_db(&database).await;
        let passes = vec![selected_pass_with_session(
            &db,
            "",
            Box::new(SelectedMediaSession(Arc::new(Vec::new()))),
        )];
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("clean full enumeration");
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.sync_token.as_deref(), Some("fixture-token"));

        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.total_assets, 1);
        assert_eq!(summary.source_deleted, 0);
        assert_eq!(summary.pending + summary.failed, 1);
        assert_eq!(summary.awaiting_provider_verification, 1);
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.as_ref(), "ABSENT_FROM_FULL_QUERY");
    }
}

#[tokio::test]
async fn incremental_legacy_pending_master_applies_explicit_provider_deletion() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    let record = TestAssetRecord::new("PENDING_CONFIRMED_DELETED")
        .filename("confirmed-deleted.jpg")
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    let media = dir.path().join("retained.jpg");
    std::fs::write(&media, b"retained historical media").unwrap();
    // Explicit source evidence arrives through current discovery, rather than
    // scheduling a lookup solely because an aggregate row is pending.
    let mut deleted = hard_deleted_change_record("PENDING_CONFIRMED_DELETED");
    deleted["recordType"] = json!("CPLMaster");
    let result = run_date_bounded_incremental_sync(db.clone(), vec![deleted]).await;

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.awaiting_provider_verification, 0);
    assert_eq!(summary.source_deleted, 1);
    assert!(!result.full_enumeration_ran);
    drop(db);
    let reopened = open_fixture_db(&database).await;
    assert_eq!(reopened.get_summary().await.unwrap().source_deleted, 1);
    assert_eq!(std::fs::read(media).unwrap(), b"retained historical media");
}

#[derive(Clone)]
struct InventoryDeletionSession {
    deletion: Value,
    delta_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for InventoryDeletionSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            let first = self
                .delta_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0;
            return Ok(changes_zone_response(
                if first {
                    vec![self.deletion.clone()]
                } else {
                    Vec::new()
                },
                "source-after",
            ));
        }
        SelectedMediaSession(Arc::new(Vec::new()))
            .post(url, body, headers)
            .await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn bounded_full_sync_revalidates_explicit_provider_deletion() {
    let dir = TempDir::new().expect("temp dir");
    let db = open_fixture_db(&dir.path().join("state.db")).await;
    let record = TestAssetRecord::new("BOUNDED_FULL_PENDING_DELETED")
        .filename("bounded-full-pending-deleted.jpg")
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    let session = InventoryDeletionSession {
        deletion: hard_deleted_change_record("BOUNDED_FULL_PENDING_DELETED"),
        delta_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let passes = vec![selected_pass_with_session(&db, "", Box::new(session))];

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "source-before".into(),
    };
    config.inventory_reconciliation = true;
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        Utc.timestamp_opt(1_746_994_800, 0).unwrap(),
    ));

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("bounded full sync should revalidate pending work");

    assert!(result.full_enumeration_ran);
    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.awaiting_provider_verification, 0);
    assert_eq!(summary.source_deleted, 1);
}

#[tokio::test]
async fn bounded_full_sync_preserves_unselected_legacy_pending_master() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone)]
    struct EmptyCurrentSelection;
    #[async_trait::async_trait]
    impl PhotosSession for EmptyCurrentSelection {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            assert!(
                !url.contains("/records/lookup?"),
                "inactive history must not cause provider lookups"
            );
            SelectedMediaSession(Arc::new(Vec::new()))
                .post(url, body, headers)
                .await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    for (added_seconds, other_child) in [
        (Some(1_700_000_000), false),
        (Some(1_640_995_200), false),
        (None, false),
        (Some(1_700_000_000), true),
    ] {
        let server = crate::start_wiremock_or_skip!();
        let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        Mock::given(method("GET"))
            .and(path("/legacy-pending.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(0)
            .mount(&server)
            .await;

        let state = TempDir::new().unwrap();
        let database = state.path().join("state.db");
        let db = open_fixture_db(&database).await;
        let mut record = TestAssetRecord::new("LEGACY_PENDING_PRESENT")
            .filename("legacy-pending.jpg")
            .checksum(&checksum)
            .size(8)
            .build();
        record.added_at =
            added_seconds.and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0));
        db.upsert_seen(&record).await.expect("seed pending row");
        if other_child {
            db.upsert_asset_master_mapping(
                "PrimarySync",
                "missing-historical-child",
                "LEGACY_PENDING_PRESENT",
            )
            .await
            .unwrap();
            db.upsert_asset_master_mapping(
                "PrimarySync",
                "asset-LEGACY_PENDING_PRESENT",
                "LEGACY_PENDING_PRESENT",
            )
            .await
            .unwrap();
        }

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.sync_mode = SyncMode::Full;
        let mappings = db.get_asset_master_mappings().await.unwrap();
        drop(db);
        for _ in 0..3 {
            let db = open_fixture_db(&database).await;
            config.state_db = Some(db.clone());
            let passes = vec![selected_pass_with_session(
                &db,
                "",
                Box::new(EmptyCurrentSelection),
            )];
            let result = download_photos_with_sync(
                &Client::new(),
                &passes,
                Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .expect("inactive historical evidence must not create current work");
            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "{result:?}"
            );
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(result.sync_token.as_deref(), Some("fixture-token"));
            assert_eq!(db.get_pending().await.unwrap()[0].added_at, record.added_at);
            assert!(
                db.get_legacy_master_state_owners()
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(db.get_summary().await.unwrap().source_deleted, 0);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
            assert_eq!(db.get_asset_master_mappings().await.unwrap(), mappings);
            let summary = db.get_summary().await.unwrap();
            assert_eq!(summary.pending, 1);
            assert_eq!(summary.failed, 0);
            assert_eq!(summary.downloaded, 0);
            assert!(server.received_requests().await.unwrap().is_empty());
            config.state_db = None;
        }
    }
}

#[tokio::test]
async fn bounded_full_sync_preserves_filtered_legacy_pending_file() {
    use crate::state::DownloadContextStateStore;
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    let mut records = incremental_photo_records_with_url(
        "LEGACY_FILTERED_ON_DISK",
        "IMG_8905.HEIC",
        "https://p01.icloud-content.com/IMG_8905.HEIC",
        16,
    );
    records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/IMG_8905.MOV",
        "fileChecksum": checksum,
        "size": 8,
    }});
    records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value": "com.apple.quicktime-movie"});
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let passes = vec![selected_pass_with_session(
        &db,
        "",
        Box::new(SelectedMediaSession(Arc::new(records.clone()))),
    )];
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Full;
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
    ));
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let expected_path = filter::expected_paths_for(&asset, &config)
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::LiveOriginal)
        .expect("legacy live photo should derive its MOV path")
        .path;
    let pending_filename = expected_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("expected MOV path should have a filename");
    let record = TestAssetRecord::new("LEGACY_FILTERED_ON_DISK")
        .version_size(VersionSizeKey::LiveOriginal)
        .filename(pending_filename)
        .checksum(&checksum)
        .size(8)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    tokio::fs::create_dir_all(expected_path.parent().expect("expected path parent"))
        .await
        .expect("create expected path parent");
    tokio::fs::write(&expected_path, &body)
        .await
        .expect("seed existing media");

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("filtered historical file must not create adoption work");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.awaiting_provider_verification, 0);
    assert_eq!(std::fs::read(&expected_path).unwrap(), body);
    assert!(db.get_downloaded_path_records().await.unwrap().is_empty());
    config.state_db = None;
    drop(passes);
    drop(db);
    for _ in 0..2 {
        let db = open_fixture_db(&database).await;
        config.state_db = Some(db.clone());
        let passes = vec![selected_pass_with_session(
            &db,
            "",
            Box::new(SelectedMediaSession(Arc::new(records.clone()))),
        )];
        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(db.get_summary().await.unwrap().pending, 1);
        assert!(db.get_downloaded_path_records().await.unwrap().is_empty());
        assert_eq!(std::fs::read(&expected_path).unwrap(), body);
        config.state_db = None;
    }
}

#[tokio::test]
async fn bounded_full_sync_revalidates_policy_excluded_asset_after_later_deletion() {
    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    let record = TestAssetRecord::new("LEGACY_FILTERED_MISSING")
        .filename("legacy-filtered-missing.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    // Retain an older aggregate exclusion as historical evidence. Current
    // selection owns its exclusion decision without reactivating that row.
    assert!(
        db.mark_policy_excluded("PrimarySync", "LEGACY_FILTERED_MISSING", "original")
            .await
            .unwrap()
    );
    db.upsert_asset_master_mapping(
        "PrimarySync",
        "asset-LEGACY_FILTERED_MISSING",
        "LEGACY_FILTERED_MISSING",
    )
    .await
    .unwrap();
    db.backdate_last_seen(
        "LEGACY_FILTERED_MISSING",
        Utc::now().timestamp().saturating_sub(86_400),
    );

    let mut live_records =
        mock_photo_records_with_filename("LEGACY_FILTERED_MISSING", "legacy-filtered-missing.jpg");
    live_records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let passes = vec![selected_pass_with_session(
        &db,
        "",
        Box::new(SelectedMediaSession(Arc::new(live_records.clone()))),
    )];

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Full;
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
    ));

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("filtered live pending asset should be policy-excluded");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.policy_excluded, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.awaiting_provider_verification, 0);
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-LEGACY_FILTERED_MISSING")
            .await
            .expect("mapping lookup")
            .as_deref(),
        Some("LEGACY_FILTERED_MISSING")
    );

    let present_passes = vec![selected_pass_with_session(
        &db,
        "",
        Box::new(SelectedMediaSession(Arc::new(live_records))),
    )];
    let present_result = download_photos_with_sync(
        &Client::new(),
        &present_passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("present filtered asset should remain policy-excluded");
    assert!(matches!(present_result.outcome, DownloadOutcome::Success));
    assert_eq!(present_result.sync_token, None);
    let summary = db.get_summary().await.expect("present summary");
    assert_eq!(summary.policy_excluded, 1);
    assert_eq!(summary.source_deleted, 0);

    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "fixture-token".into(),
    };
    config.inventory_reconciliation = true;
    let deleted_passes = vec![selected_pass_with_session(
        &db,
        "",
        Box::new(InventoryDeletionSession {
            deletion: hard_deleted_change_record("LEGACY_FILTERED_MISSING"),
            delta_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }),
    )];
    let deleted_result = download_photos_with_sync(
        &Client::new(),
        &deleted_passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("explicit provider deletion should supersede policy exclusion");

    assert!(matches!(deleted_result.outcome, DownloadOutcome::Success));
    assert_eq!(deleted_result.sync_token.as_deref(), Some("source-after"));
    let summary = db.get_summary().await.expect("deleted summary");
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.policy_excluded, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.source_deleted, 1);
}

#[tokio::test]
async fn contract_unknown_provider_identity_remains_pending_without_durable_match() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("LEGACY_AMBIGUOUS")
        .checksum("no-sibling-matches")
        .size(999)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");

    let master = mock_master_record_with_filename("LEGACY_AMBIGUOUS", "ambiguous.jpg");
    let hydration_records = vec![
        master.clone(),
        mock_asset_record_for("asset-ambiguous-a", "LEGACY_AMBIGUOUS"),
        mock_asset_record_for("asset-ambiguous-b", "LEGACY_AMBIGUOUS"),
    ];
    let session = LegacyPendingHydrationSession {
        lookup_records: Arc::new(vec![master]),
        hydration_records: Arc::new(hydration_records),
        hydration_error: None,
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());

    let plan = build_pending_retry_download_tasks(
        &passes,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .expect("non-matching siblings should remain safely unresolved");

    assert!(plan.tasks.is_empty());
    assert_eq!(plan.unmatched_targets.len(), 1);
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.awaiting_provider_verification, 1);
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-ambiguous-a")
            .await
            .expect("mapping lookup"),
        None
    );
}

#[tokio::test]
async fn pending_direct_lookup_requires_rendition_evidence_before_claiming_owner() {
    let matching_checksum = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    for (checksum, size, rejected) in [
        ("different-checksum", 1024, true),
        (matching_checksum, 999, true),
        ("different-checksum", 999, true),
        (matching_checksum, 1024, false),
    ] {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let record = TestAssetRecord::new("LEGACY_DIRECT")
            .checksum(checksum)
            .size(size)
            .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-direct", "LEGACY_DIRECT")
            .await
            .unwrap();
        let mut child = mock_asset_record_for("asset-direct", "LEGACY_DIRECT");
        child["fields"]["addedDate"] = json!({"value": 1_700_000_000_123i64, "type": "TIMESTAMP"});
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session(
                "PrimarySync",
                "",
                Box::new(PendingLookupSession {
                    records: Arc::new(vec![
                        mock_master_record_with_filename("LEGACY_DIRECT", "direct.jpg"),
                        child,
                    ]),
                }),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let dir = TempDir::new().unwrap();
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        for _cycle in 0..2 {
            let plan = build_pending_retry_download_tasks(
                &passes,
                &config,
                DownloadRunMode::Download,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(plan.tasks.len(), usize::from(!rejected));
            assert_eq!(plan.identity_incomplete, rejected);
            let owners = db.get_legacy_master_state_owners().await.unwrap();
            assert_eq!(owners.len(), usize::from(!rejected));
            if rejected {
                assert_eq!(plan.unmatched_targets.len(), 1);
                let pending = db.get_pending().await.unwrap();
                assert_eq!(pending[0].checksum.as_ref(), checksum);
                assert_eq!(pending[0].size_bytes, size);
                assert_eq!(pending[0].added_at, record.added_at);
                assert_eq!(db.get_summary().await.unwrap().source_deleted, 0);
            }
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
    }
}

#[tokio::test]
async fn pending_retry_uses_legacy_owner_to_resolve_matching_siblings() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("LEGACY_OWNED_RETRY")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .filename("owned-retry.jpg")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "LEGACY_OWNED_RETRY", "asset-owned-b",)
            .await
            .expect("claim legacy owner")
    );

    let master = mock_master_record_with_filename("LEGACY_OWNED_RETRY", "owned-retry.jpg");
    let session = LegacyPendingHydrationSession {
        lookup_records: Arc::new(vec![
            master.clone(),
            mock_asset_record_for("asset-owned-b", "LEGACY_OWNED_RETRY"),
        ]),
        hydration_records: Arc::new(vec![
            master,
            mock_asset_record_for("asset-owned-a", "LEGACY_OWNED_RETRY"),
            mock_asset_record_for("asset-owned-b", "LEGACY_OWNED_RETRY"),
        ]),
        hydration_error: None,
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db);

    let plan = build_pending_retry_download_tasks(
        &passes,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .expect("persisted owner should resolve matching siblings");

    assert_eq!(plan.tasks.len(), 1);
    assert_eq!(plan.unmatched_targets.len(), 0);
    assert_eq!(plan.tasks[0].asset_id.as_ref(), "LEGACY_OWNED_RETRY");
    assert_eq!(plan.tasks[0].asset_record_name.as_ref(), "asset-owned-b");
}

#[tokio::test]
async fn pending_retry_retains_transient_legacy_hydration_failure() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("LEGACY_TRANSIENT").build();
    db.upsert_seen(&record).await.expect("seed pending row");
    let session = LegacyPendingHydrationSession {
        lookup_records: Arc::new(vec![mock_master_record_with_filename(
            "LEGACY_TRANSIENT",
            "transient.jpg",
        )]),
        hydration_records: Arc::new(Vec::new()),
        hydration_error: Some(Arc::from("temporary changes failure")),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    config.state_db = Some(db.clone());

    let plan = build_pending_retry_download_tasks(
        &passes,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .expect("transient hydration failure should retain durable work");

    assert!(plan.tasks.is_empty());
    assert!(plan.identity_incomplete);
    assert_eq!(plan.unmatched_targets.len(), 1);
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.awaiting_provider_verification, 1);
}

#[tokio::test]
async fn pending_retry_deleted_sibling_does_not_tombstone_present_master_state() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("MASTER_WITH_SIBLINGS")
        .filename("master-with-siblings.jpg")
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.upsert_asset_master_mapping("PrimarySync", "asset-a-deleted", "MASTER_WITH_SIBLINGS")
        .await
        .expect("seed deleted sibling mapping");
    db.upsert_asset_master_mapping("PrimarySync", "asset-b-present", "MASTER_WITH_SIBLINGS")
        .await
        .expect("seed present sibling mapping");

    let session = PendingLookupSession {
        records: Arc::new(vec![
            json!({
                "recordName": "asset-a-deleted",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            }),
            mock_master_record_with_filename("MASTER_WITH_SIBLINGS", "master-with-siblings.jpg"),
            mock_asset_record_for("asset-b-present", "MASTER_WITH_SIBLINGS"),
        ]),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());

    let plan = build_pending_retry_download_tasks(
        &passes,
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .expect("build pending retry plan");

    assert!(plan.tasks.is_empty());
    assert_eq!(plan.unmatched_targets.len(), 1);
    assert!(plan.identity_incomplete);
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.source_deleted, 0);
    assert_eq!(summary.pending, 1);
}

async fn run_date_bounded_incremental_sync(
    db: Arc<crate::state::SqliteStateDb>,
    records: Vec<Value>,
) -> SyncResult {
    #[derive(Clone)]
    struct DeletionSession(Arc<Vec<Value>>);
    #[async_trait::async_trait]
    impl PhotosSession for DeletionSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                return Ok(changes_zone_response(
                    self.0.as_ref().clone(),
                    "zone-token-next",
                ));
            }
            SelectedMediaSession(Arc::new(Vec::new()))
                .post(url, body, headers)
                .await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    let session = DeletionSession(Arc::new(records));
    let passes = vec![selected_pass_with_session(
        &db,
        "",
        Box::new(session.clone()),
    )];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        Utc.timestamp_opt(1_746_994_800, 0).unwrap(),
    ));
    super::super::test_support::initialize_empty_selection(&db, &config).await;

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("bounded incremental sync should complete");
    if !result.full_enumeration_ran
        && result.sync_token.is_some()
        && matches!(result.outcome, DownloadOutcome::Success)
    {
        let database = db.path().to_path_buf();
        drop(passes);
        drop(db);
        for replay in [session, DeletionSession(Arc::new(Vec::new()))] {
            config.state_db = None;
            let reopened = open_fixture_db(&database).await;
            config.state_db = Some(reopened.clone());
            let passes = vec![selected_pass_with_session(&reopened, "", Box::new(replay))];
            let quiet = download_photos_with_sync(
                &Client::new(),
                &passes,
                Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(quiet.outcome, DownloadOutcome::Success),
                "{quiet:?}"
            );
            assert!(!quiet.full_enumeration_ran, "{quiet:?}");
            assert_eq!(quiet.sync_token, result.sync_token);
            assert_eq!(quiet.stats.downloaded, 0);
            assert!(reopened.get_pending().await.unwrap().is_empty());
        }
    }
    result
}

#[tokio::test]
async fn empty_selection_revalidates_unknown_tombstones_and_holds_malformed_relations() {
    for record_type in [Value::Null, json!("CPLContainerRelation")] {
        let state = TempDir::new().unwrap();
        let db = open_fixture_db(&state.path().join("state.db")).await;
        let mut record = hard_deleted_change_record("UNKNOWN_OR_RELATION");
        if !record_type.is_null() {
            // Even a colliding historical ID cannot turn a typed relation
            // tombstone into an irrelevant asset deletion.
            db.upsert_seen(&TestAssetRecord::new("UNKNOWN_OR_RELATION").build())
                .await
                .unwrap();
            db.mark_soft_deleted("PrimarySync", "UNKNOWN_OR_RELATION", None)
                .await
                .unwrap();
        }
        let malformed_relation = !record_type.is_null();
        record["recordType"] = record_type;
        let result = run_date_bounded_incremental_sync(db, vec![record]).await;
        if malformed_relation {
            assert!(result.sync_token.is_none());
            assert_eq!(
                result.stats.sync_token_blocked_reason,
                Some("unparsable_relation_delta")
            );
            continue;
        }
        assert!(
            result.full_enumeration_ran,
            "unknown membership must revalidate selection: {result:?}"
        );
    }
}

#[tokio::test]
async fn incremental_source_delete_prunes_pending_row_in_bounded_sync() {
    let state = TempDir::new().unwrap();
    let db = open_fixture_db(&state.path().join("state.db")).await;
    let record = TestAssetRecord::new("PENDING_SOURCE_DELETED")
        .filename("pending-source-deleted.jpg")
        .checksum("ck_pending_source_deleted")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");

    let result = run_date_bounded_incremental_sync(
        db.clone(),
        vec![hard_deleted_change_record("PENDING_SOURCE_DELETED")],
    )
    .await;

    assert!(
        !result.full_enumeration_ran,
        "source delete cleanup should not force full enumeration: {result:?}"
    );
    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(!result.stats.sync_token_blocked);
    assert!(db.get_pending().await.expect("pending page").is_empty());
}

#[tokio::test]
async fn incremental_prunes_existing_source_deleted_pending_row_without_wide_sync() {
    let state = TempDir::new().unwrap();
    let db = open_fixture_db(&state.path().join("state.db")).await;
    let record = TestAssetRecord::new("STALE_SOURCE_DELETED")
        .filename("stale-source-deleted.jpg")
        .checksum("ck_stale_source_deleted")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.mark_soft_deleted("PrimarySync", "STALE_SOURCE_DELETED", None)
        .await
        .expect("simulate old source-deleted pending row");

    let result = run_date_bounded_incremental_sync(db.clone(), Vec::new()).await;

    assert!(
        !result.full_enumeration_ran,
        "cleanup should not force full enumeration"
    );
    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(!result.stats.sync_token_blocked);
    assert!(db.get_pending().await.expect("pending page").is_empty());
}

#[tokio::test]
async fn incremental_ignores_pending_rows_from_other_libraries() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    let record = crate::test_helpers::TestAssetRecord::new("SHARED_PENDING_BEFORE_SYNC")
        .library("SharedSync-ONE")
        .filename("shared-pending-before-sync.jpg")
        .checksum("ck_shared_pending_before_sync")
        .size(1024)
        .build();
    db.upsert_seen(&record)
        .await
        .expect("seed shared pending row");

    let session = MockPhotosFlow::new()
        .changes_zone_page(Vec::new(), "zone-token-next", false)
        .build();
    let passes = vec![selected_pass_with_session(&db, "", Box::new(session))];

    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    initialize_empty_selection(&db, &config).await;

    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("other-library pending rows must not force full enumeration");

    assert!(
        !result.full_enumeration_ran,
        "PrimarySync should remain incremental when only SharedSync has pending work"
    );
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    drop(passes);
    drop(db);
    let reopened = open_fixture_db(&database).await;
    let pending = reopened.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].library.as_ref(), "SharedSync-ONE");
    assert_eq!(pending[0].id.as_ref(), "SHARED_PENDING_BEFORE_SYNC");
}

#[tokio::test]
async fn metadata_backfill_in_another_library_does_not_force_full_enumeration() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("state.db");
    let db = open_fixture_db(&database).await;
    let media = dir.path().join("other.jpg");
    std::fs::write(&media, b"other library media").unwrap();
    let row = TestAssetRecord::new("OTHER_LIBRARY_BACKFILL")
        .library("SharedSync-OTHER")
        .build();
    db.upsert_seen(&row).await.unwrap();
    db.mark_downloaded(
        "SharedSync-OTHER",
        "OTHER_LIBRARY_BACKFILL",
        "original",
        &media,
        "hash",
        None,
    )
    .await
    .unwrap();
    db.clear_metadata_hash_for_test("SharedSync-OTHER", "OTHER_LIBRARY_BACKFILL", "original");
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        Utc.timestamp_opt(1_746_994_800, 0).unwrap(),
    ));
    initialize_empty_selection(&db, &config).await;
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    let session = MockPhotosFlow::new()
        .changes_zone_page(Vec::new(), "zone-token-next", false)
        .build();
    let passes = vec![selected_pass_with_session(&db, "", Box::new(session))];
    let result = download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!result.full_enumeration_ran);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(
        db.has_downloaded_without_metadata_hash("SharedSync-OTHER")
            .await
            .unwrap()
    );
    drop(passes);
    drop(db);
    let reopened = open_fixture_db(&database).await;
    assert!(
        reopened
            .has_downloaded_without_metadata_hash("SharedSync-OTHER")
            .await
            .unwrap()
    );
    assert_eq!(std::fs::read(media).unwrap(), b"other library media");
}

#[tokio::test]
async fn hidden_invalid_capture_date_preserves_pending_recovery() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone)]
    struct HiddenSelectionSession(Vec<Value>);
    #[async_trait::async_trait]
    impl PhotosSession for HiddenSelectionSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            // Sibling hydration is source evidence, not selected membership.
            let records = if url.contains("/records/lookup?") {
                self.0.clone()
            } else {
                self.0[..2].to_vec()
            };
            SelectedMediaSession(Arc::new(records))
                .post(url, body, headers)
                .await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    for invalid in [None, Some(Value::Null), Some(json!(1e100)), Some(json!(0))] {
        let valid_epoch = invalid == Some(json!(0));
        for sibling_hidden in [None, Some(false), Some(true)] {
            let with_sibling = sibling_hidden.is_some();
            let server = crate::start_wiremock_or_skip!();
            let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
            let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
            Mock::given(method("GET"))
                .and(path("/retained.jpg"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(body.clone())
                        .insert_header("content-type", "image/jpeg"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let dir = TempDir::new().unwrap();
            let db_path = dir.path().join("state.db");
            let media = dir.path().join("media");
            std::fs::create_dir(&media).unwrap();
            let sentinel = media.join("unrelated.jpg");
            std::fs::write(&sentinel, b"retained unrelated media").unwrap();
            let record = TestAssetRecord::new("LEGACY_INVALID_CAPTURE")
                .filename("retained.jpg")
                .checksum(&checksum)
                .size(8)
                .created_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
                .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
                .build();
            {
                let db = open_fixture_db(&db_path).await;
                db.upsert_seen(&record).await.unwrap();
                db.set_asset_verification(
                    "PrimarySync",
                    "LEGACY_INVALID_CAPTURE",
                    "original",
                    crate::state::AssetVerificationState::Unknown,
                    "prior incomplete identity",
                )
                .await
                .unwrap();
                // This is a known pending identity, not an unmapped historical
                // master offered as new retry work. Invalid current dates must
                // still refuse it; a competing sibling still prevents a claim.
                db.upsert_asset_master_mapping(
                    "PrimarySync",
                    "asset-LEGACY_INVALID_CAPTURE",
                    "LEGACY_INVALID_CAPTURE",
                )
                .await
                .unwrap();
                if with_sibling {
                    db.upsert_asset_master_mapping(
                        "PrimarySync",
                        "valid-sibling",
                        "LEGACY_INVALID_CAPTURE",
                    )
                    .await
                    .unwrap();
                }
            }
            for cycle in 0..4 {
                eprintln!(
                    "pending date={invalid:?} sibling_hidden={sibling_hidden:?} cycle={cycle}: seed -> malformed/control -> reopen -> valid -> steady"
                );
                let recovered = !with_sibling && (valid_epoch || cycle >= 2);
                let selected_ready = valid_epoch || cycle >= 2;
                let db = open_fixture_db(&db_path).await;
                let mut records = incremental_photo_records_with_url(
                    "LEGACY_INVALID_CAPTURE",
                    "retained.jpg",
                    &format!("{}/retained.jpg", server.uri()),
                    8,
                );
                records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
                records[1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                    json!("_defaultOwner");
                records[1]["fields"]["isHidden"] = json!({"value": 1, "type": "INT64"});
                if cycle < 2 || valid_epoch {
                    match &invalid {
                        None => {
                            records[1]["fields"]
                                .as_object_mut()
                                .unwrap()
                                .remove("assetDate");
                        }
                        Some(value) => {
                            records[1]["fields"]["assetDate"] =
                                json!({"value": value, "type": "TIMESTAMP"});
                        }
                    }
                }
                if with_sibling {
                    let mut sibling = records[1].clone();
                    sibling["recordName"] = json!("valid-sibling");
                    sibling["fields"]["isHidden"]["value"] =
                        json!(i64::from(sibling_hidden.unwrap()));
                    sibling["fields"]["assetDate"] =
                        json!({"value": 1_700_000_000_000i64, "type": "TIMESTAMP"});
                    records.push(sibling);
                }
                let passes = vec![
                    super::super::test_support::selected_smart_pass_with_session(
                        &db,
                        "Hidden",
                        Box::new(HiddenSelectionSession(records)),
                    ),
                ];
                let mut config = test_config();
                config.directory = Arc::from(media.as_path());
                config.state_db = Some(db.clone());
                config.sync_mode = SyncMode::Full;
                config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
                    Utc.timestamp_opt(if valid_epoch { 0 } else { 946_684_800 }, 0)
                        .unwrap(),
                ));
                let result = download_photos_with_sync(
                    &Client::new(),
                    &passes,
                    Arc::new(config),
                    DownloadControls::download_hidden(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                drop(db);
                let db = open_fixture_db(&db_path).await;
                let summary = db.get_summary().await.unwrap();
                assert_eq!(
                    summary.pending,
                    u64::from(!recovered),
                    "invalid={invalid:?}, sibling={with_sibling}, cycle={cycle}"
                );
                assert_eq!(summary.policy_excluded, 0);
                assert_eq!(summary.source_deleted, 0);
                assert_eq!(summary.downloaded, u64::from(selected_ready));
                if selected_ready {
                    assert!(
                        matches!(result.outcome, DownloadOutcome::Success),
                        "{result:?}"
                    );
                }
                if !selected_ready {
                    assert!(
                        result.checkpoint.identity_incomplete
                            || result.checkpoint.sync_token_blocked,
                        "malformed or unresolved selected identity must hold progress: {result:?}"
                    );
                    assert_eq!(result.sync_token, None);
                }
                assert_eq!(
                    result.stats.downloaded,
                    usize::from(if valid_epoch { cycle == 0 } else { cycle == 2 })
                );
                assert_eq!(
                    db.get_legacy_master_state_owners().await.unwrap(),
                    if recovered {
                        std::collections::HashSet::from([(
                            "PrimarySync".to_string(),
                            "LEGACY_INVALID_CAPTURE".to_string(),
                            "asset-LEGACY_INVALID_CAPTURE".to_string(),
                        )])
                    } else {
                        std::collections::HashSet::new()
                    }
                );
                if recovered {
                    let rows = db.get_downloaded_page(0, 10).await.unwrap();
                    assert_eq!(
                        std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
                        body
                    );
                    assert_eq!(
                        rows[0].created_at,
                        if valid_epoch {
                            Utc.timestamp_opt(0, 0).unwrap()
                        } else {
                            record.created_at
                        }
                    );
                    assert_eq!(rows[0].checksum.as_ref(), checksum);
                } else {
                    let rows = db.get_pending().await.unwrap();
                    assert_eq!(rows[0].created_at, record.created_at);
                    assert_eq!(rows[0].added_at, record.added_at);
                    assert_eq!(summary.awaiting_provider_verification, 1);
                }
                if selected_ready && with_sibling {
                    let rows = db.get_downloaded_page(0, 10).await.unwrap();
                    assert_eq!(rows[0].id.as_ref(), "asset-LEGACY_INVALID_CAPTURE");
                    assert_eq!(
                        std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
                        body
                    );
                    assert_eq!(rows[0].checksum.as_ref(), checksum);
                }
                assert_eq!(
                    std::fs::read(&sentinel).unwrap(),
                    b"retained unrelated media"
                );
            }
            server.verify().await;
        }
    }
}
