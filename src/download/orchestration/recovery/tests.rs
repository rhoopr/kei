use crate::state::SqliteStateDb;
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
    PendingLookupSession, album_with_session, changes_zone_response, hard_deleted_change_record,
    incremental_photo_records_with_url, mock_album, mock_asset_record_for,
    mock_master_record_with_filename, mock_photo_records_with_filename, retry_test_task,
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
struct BoundedPolicyLookupSession {
    records: Arc<Vec<Value>>,
}

#[async_trait::async_trait]
impl PhotosSession for BoundedPolicyLookupSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            return Ok(json!({"records": self.records.as_ref().clone()}));
        }
        if url.contains("/changes/zone?") {
            return Ok(json!({
                "zones": [{
                    "zoneID": {
                        "zoneName": "PrimarySync",
                        "ownerRecordName": "_defaultOwner"
                    },
                    "moreComing": false,
                    "records": []
                }]
            }));
        }
        Ok(json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
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
    use sha1::{Digest, Sha1};

    for verified in [true, false] {
        let dir = TempDir::new().expect("temp dir");
        let database = dir.path().join("state.db");
        let db = Arc::new(
            crate::state::SqliteStateDb::open(&database)
                .await
                .expect("state db"),
        );
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.file_match_policy = FileMatchPolicy::NameId7;

        let body = b"12345678";
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(body);
        let signature = data_encoding::BASE64.encode(&[vec![1], hash.finalize().to_vec()].concat());
        let checksum = if verified {
            signature.as_str()
        } else {
            "opaque-unverified"
        };
        let mut records = incremental_photo_records_with_url(
            "ID7_ADOPT",
            "id7.jpg",
            "https://p01.icloud-content.com/id7.jpg",
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let recorded_path = filter::expected_paths_for(&asset, &config)
            .into_iter()
            .next()
            .expect("asset should derive a path")
            .path;
        tokio::fs::create_dir_all(recorded_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&recorded_path, body).await.unwrap();
        seed_pending_retry_with_recorded_path(db.as_ref(), &asset, checksum, 8, &recorded_path)
            .await;
        drop(config);
        drop(db);

        for stage in 0..4 {
            let current_verified = verified || stage >= 2;
            if current_verified {
                records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(signature);
            }
            let db = Arc::new(crate::state::SqliteStateDb::open(&database).await.unwrap());
            if !verified && stage == 2 {
                // A normal provider observation must update durable pending identity
                // before targeted retry can match a changed generation.
                let observed = TestAssetRecord::new(asset.state_id())
                    .filename(recorded_path.file_name().unwrap().to_str().unwrap())
                    .checksum(&signature)
                    .size(8)
                    .added_at(asset.added_date())
                    .build();
                db.upsert_seen(&observed).await.unwrap();
            }
            let mut config = test_config();
            config.directory = Arc::from(dir.path());
            config.state_db = Some(db.clone());
            config.file_match_policy = FileMatchPolicy::NameId7;
            let passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session(
                    "PrimarySync",
                    "",
                    Box::new(PendingLookupSession {
                        records: Arc::new(records.clone()),
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
            assert!(plan.tasks.is_empty());
            let summary = db.get_summary().await.expect("state summary");
            assert_eq!(summary.downloaded, u64::from(current_verified));
            assert_eq!(summary.pending, u64::from(!current_verified));
            assert_eq!(summary.policy_excluded, 0);
            assert_eq!(summary.failed, 0);
            assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), body);
            let (provider, local, source): (String, Option<String>, Option<String>) = db
                .acquire_lock("inspect recovery provenance")
                .unwrap()
                .query_row(
                    "SELECT provider_checksum, local_checksum, source_checksum FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4 AND provider_checksum=?5",
                    rusqlite::params!["PrimarySync", asset.state_id(), VersionSizeKey::Original.as_str(), recorded_path.to_str().unwrap(), if current_verified { signature.as_str() } else { "opaque-unverified" }],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            if current_verified {
                let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(
                    downloaded[0].local_path.as_deref(),
                    Some(recorded_path.as_path())
                );
                let sha256 = file::compute_sha256(&recorded_path).await.unwrap();
                assert_eq!(
                    downloaded[0].local_checksum.as_deref(),
                    Some(sha256.as_str())
                );
                assert_eq!(
                    downloaded[0].download_checksum.as_deref(),
                    Some(sha256.as_str())
                );
                assert_eq!(provider, signature);
                assert_eq!(local.as_deref(), Some(sha256.as_str()));
                assert_eq!(source.as_deref(), Some(sha256.as_str()));
            } else {
                assert_eq!(provider, "opaque-unverified");
                assert_eq!(local.as_deref(), Some("previous-local-checksum"));
                assert_eq!(source, None);
                assert_eq!(plan.unmatched_targets.len(), 1);
            }
        }
    }
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

    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
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
    let session = PendingLookupSession {
        records: Arc::new(records),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session("PrimarySync", "AlbumOne", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
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
    changed_resource_repair("normal").await;
}

#[tokio::test]
async fn changed_resource_repair_collision_failure_reopens_without_duplicate() {
    changed_resource_repair("collision").await;
}

#[tokio::test]
async fn changed_resource_repair_refuses_wrong_child() {
    changed_resource_repair("wrong-child").await;
}

#[tokio::test]
async fn changed_resource_repair_admission_failure_reopens_same_reservation() {
    changed_resource_repair("admission").await;
}

#[tokio::test]
async fn changed_resource_repair_refuses_wrong_source() {
    changed_resource_repair("wrong-source").await;
}

#[tokio::test]
async fn changed_resource_repair_requires_explicit_option() {
    changed_resource_repair("disabled").await;
}

#[tokio::test]
async fn changed_resource_repair_refuses_post_match_file_change() {
    changed_resource_repair("mutation").await;
}

async fn changed_resource_repair(scenario: &str) {
    let fail_first = scenario == "collision";
    let wrong_child = scenario == "wrong-child";
    let wrong_source = scenario == "wrong-source";
    let interrupt = scenario == "admission";
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let current_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let current_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&current_body));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let response_calls = calls.clone();
    let response_body = current_body.clone();
    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(move |_: &wiremock::Request| {
            let call = response_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if fail_first && call == 0 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_bytes(response_body.clone())
            }
        })
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("state.db");
    let owner = crate::state::db::account::AccountOwner::authenticated(
        "repair@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"repair-fixture"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
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

    let original_time = std::fs::metadata(&recorded_path)
        .unwrap()
        .modified()
        .unwrap();
    let collision = recorded_path.with_file_name(crate::download::paths::add_dedup_suffix(
        "photo.jpg",
        current_body.len() as u64,
    ));
    if fail_first {
        std::fs::write(&collision, b"unrelated collision").unwrap();
    }
    let collision_time =
        fail_first.then(|| std::fs::metadata(&collision).unwrap().modified().unwrap());
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
    if wrong_child {
        records[1]["recordName"] = json!("different-child");
    }
    if wrong_source {
        for record in &mut records {
            record["zoneID"] =
                json!({"zoneName":"SharedSync-foreign","ownerRecordName":"_defaultOwner"});
        }
    }
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session(
            "PrimarySync",
            "AlbumOne",
            Box::new(PendingLookupSession {
                records: Arc::new(records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.repair_truncated = scenario != "disabled";
    config.retry.max_retries = 0;
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };

    let mut reserved = None;
    if interrupt {
        db.acquire_lock("inject changed repair observation failure").unwrap().execute_batch(
            "CREATE TRIGGER interrupt_repair_observation BEFORE UPDATE OF checksum ON assets WHEN OLD.checksum <> NEW.checksum BEGIN SELECT RAISE(ABORT,'injected repair observation failure'); END;"
        ).unwrap();
        let first = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await;
        assert!(first.as_ref().map_or(true, |result| !matches!(
            result.outcome,
            DownloadOutcome::Success
        )));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let conn = db
            .acquire_lock("durable choice before failed observation")
            .unwrap();
        reserved = Some(
            conn.query_row::<String, _, _>(
                "SELECT destination_path FROM reconciliation_paths WHERE provider_checksum=?1",
                [&current_checksum],
                |r| r.get(0),
            )
            .unwrap(),
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT checksum FROM assets WHERE id='CHANGED_TRUNCATED_PATH'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "old-provider-checksum"
        );
        conn.execute_batch("DROP TRIGGER interrupt_repair_observation")
            .unwrap();
    }
    if fail_first {
        let failed = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!matches!(failed.outcome, DownloadOutcome::Success));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&recorded_path).unwrap(), b"bad");
        reserved = Some(
            db.acquire_lock("saved sibling after failed transfer")
                .unwrap()
                .query_row::<String, _, _>(
                    "SELECT destination_path FROM reconciliation_paths WHERE provider_checksum=?1",
                    [&current_checksum],
                    |r| r.get(0),
                )
                .unwrap(),
        );
        config.state_db = None;
    }
    config.state_db = None;
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
    config.state_db = Some(db.clone());
    let client = Client::new();
    let attempt = download_photos_with_sync(
        &client,
        &passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    );
    let result = if scenario == "mutation" {
        let old = recorded_path.clone();
        crate::download::retry::AFTER_REPAIR_MATCH
            .scope(
                Arc::new(move || {
                    std::fs::write(&old, b"changed after match").unwrap();
                }),
                attempt,
            )
            .await
    } else {
        attempt.await
    }
    .expect("retry changed provider version through production pipeline");

    if wrong_child || wrong_source || matches!(scenario, "disabled" | "mutation") {
        assert!(!matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(&recorded_path).unwrap(),
            if scenario == "mutation" {
                b"changed after match".as_slice()
            } else {
                b"bad".as_slice()
            }
        );
        if scenario != "mutation" {
            assert_eq!(
                std::fs::metadata(&recorded_path)
                    .unwrap()
                    .modified()
                    .unwrap(),
                original_time
            );
        }
        assert!(
            db.get_summary().await.unwrap().pending + db.get_summary().await.unwrap().failed > 0
        );
        return;
    }
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "{result:?}; warnings={:?}",
        trace
            .events()
            .iter()
            .filter(|event| event.level <= tracing::Level::WARN)
            .collect::<Vec<_>>()
    );
    assert_eq!(result.stats.downloaded, 1);
    assert_eq!(tokio::fs::read(&recorded_path).await.unwrap(), b"bad");
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 1);
    let current_path = downloaded[0]
        .local_path
        .as_ref()
        .expect("current provider version has a local path");
    assert_ne!(current_path, &recorded_path);
    if let Some(reserved) = reserved {
        assert_eq!(current_path, &std::path::PathBuf::from(reserved));
    }
    assert_eq!(
        db.acquire_lock("old receipt remains historical")
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT provider_checksum FROM asset_metadata_paths WHERE local_path=?1",
                [recorded_path.to_str().unwrap()],
                |r| r.get(0)
            )
            .unwrap(),
        "old-provider-checksum"
    );
    assert_eq!(tokio::fs::read(current_path).await.unwrap(), current_body);
    assert_eq!(current_path.parent(), recorded_path.parent());
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    let current_path = current_path.clone();
    let current_time = std::fs::metadata(&current_path)
        .unwrap()
        .modified()
        .unwrap();
    config.state_db = None;
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
    config.state_db = Some(db.clone());
    for _ in 0..2 {
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
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(std::fs::read(&recorded_path).unwrap(), b"bad");
        assert_eq!(
            std::fs::metadata(&recorded_path)
                .unwrap()
                .modified()
                .unwrap(),
            original_time
        );
        assert_eq!(
            std::fs::metadata(&current_path)
                .unwrap()
                .modified()
                .unwrap(),
            current_time
        );
        if fail_first {
            assert_eq!(std::fs::read(&collision).unwrap(), b"unrelated collision");
            assert_eq!(
                Some(std::fs::metadata(&collision).unwrap().modified().unwrap()),
                collision_time
            );
        }
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        if fail_first { 2 } else { 1 }
    );
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

    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
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
    let retry_passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(changed_records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

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
    let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
    Mock::given(method("GET"))
        .and(path("/failed-before-sync.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .mount(&server)
        .await;

    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
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
    let session = PendingLookupSession {
        records: Arc::new(records),
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
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("PENDING_DELETED_UPSTREAM")
        .filename("pending-deleted-upstream.jpg")
        .checksum("ck_pending_deleted_upstream")
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
    config.state_db = Some(db.clone());
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
    .expect("unmatched pending rows should remain represented after targeted retry");

    assert!(
        !result.full_enumeration_ran,
        "retaining unresolved pending state should not force full enumeration"
    );
    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_blocked_reason, None);
    assert_eq!(result.stats.stale_pending_pruned, 0);
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.awaiting_provider_verification, 1);
    assert_eq!(db.get_pending().await.expect("pending page").len(), 1);
}

#[tokio::test]
async fn full_enumeration_query_absence_never_deletes_pending_state() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("ABSENT_FROM_FULL_QUERY")
        .filename("absent-from-full-query.jpg")
        .checksum("ck_absent_from_full_query")
        .size(1024)
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");

    let session = MockPhotosFlow::new()
        .empty_query_page(Some("zone-token-next"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Full;

    download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("clean full enumeration");

    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.total_assets, 1);
    assert_eq!(summary.source_deleted, 0);
    assert_eq!(summary.pending + summary.failed, 1);
    assert_eq!(summary.awaiting_provider_verification, 1);
}

#[tokio::test]
async fn incremental_legacy_pending_master_lookup_clears_explicit_provider_deletion() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("PENDING_CONFIRMED_DELETED")
        .filename("confirmed-deleted.jpg")
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    let session = PendingLookupSession {
        records: Arc::new(vec![json!({
            "recordName": "PENDING_CONFIRMED_DELETED",
            "serverErrorCode": "UNKNOWN_ITEM",
            "reason": "record not found"
        })]),
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
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("explicit provider deletion should resolve pending work");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.awaiting_provider_verification, 0);
    assert_eq!(summary.source_deleted, 1);
}

#[tokio::test]
async fn bounded_full_sync_revalidates_explicit_provider_deletion() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("BOUNDED_FULL_PENDING_DELETED")
        .filename("bounded-full-pending-deleted.jpg")
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    let session = PendingLookupSession {
        records: Arc::new(vec![json!({
            "recordName": "BOUNDED_FULL_PENDING_DELETED",
            "serverErrorCode": "UNKNOWN_ITEM",
            "reason": "record not found"
        })]),
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
    config.sync_mode = SyncMode::Full;
    config.recent = Some(300);

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
async fn bounded_full_sync_hydrates_live_legacy_pending_master() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    for (hidden, added_seconds, other_child) in [
        (false, Some(1_700_000_000), false),
        (true, Some(1_700_000_000), false),
        (true, Some(1_640_995_200), false),
        (true, None, false),
        (true, Some(1_700_000_000), true),
    ] {
        let guarded = added_seconds != Some(1_700_000_000) || other_child;
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
            .expect(u64::from(!guarded))
            .mount(&server)
            .await;

        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
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

        let download_url = format!("{}/legacy-pending.jpg", server.uri());
        let mut records = incremental_photo_records_with_url(
            "LEGACY_PENDING_PRESENT",
            "legacy-pending.jpg",
            &download_url,
            8,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
        records[1]["fields"]["addedDate"] =
            json!({"value": 1_700_000_000_123i64, "type": "TIMESTAMP"});
        records[1]["fields"]["isHidden"] = json!({"value": i64::from(hidden), "type": "INT64"});
        let session = LegacyPendingHydrationSession {
            lookup_records: Arc::new(vec![records[0].clone()]),
            hydration_records: Arc::new(records),
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
        config.sync_mode = SyncMode::Full;
        config.recent = Some(300);

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("live legacy pending master should hydrate and download");

        if guarded {
            // Retained pending debt is separate from current-source identity.
            assert!(!result.stats.identity_incomplete);
            assert!(result.stats.failed > 0);
            assert_eq!(db.get_pending().await.unwrap()[0].added_at, record.added_at);
            assert!(
                db.get_legacy_master_state_owners()
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(db.get_summary().await.unwrap().source_deleted, 0);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
            continue;
        }
        assert!(result.full_enumeration_ran);
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        let summary = db.get_summary().await.expect("summary");
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.awaiting_provider_verification, 0);
        assert_eq!(summary.source_deleted, 0);
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-LEGACY_PENDING_PRESENT")
                .await
                .expect("mapping lookup")
                .as_deref(),
            Some("LEGACY_PENDING_PRESENT")
        );
        let downloaded = db
            .get_downloaded_page(0, 10)
            .await
            .expect("downloaded page");
        let local_path = downloaded[0]
            .local_path
            .as_ref()
            .expect("downloaded row has local path");
        assert!(tokio::fs::metadata(local_path).await.is_ok());
        assert_eq!(tokio::fs::read(local_path).await.unwrap(), body);
        assert!(
            db.get_legacy_master_state_owners()
                .await
                .unwrap()
                .contains(&(
                    "PrimarySync".to_string(),
                    "LEGACY_PENDING_PRESENT".to_string(),
                    "asset-LEGACY_PENDING_PRESENT".to_string(),
                ))
        );
    }
}

#[tokio::test]
async fn bounded_full_sync_adopts_filtered_legacy_pending_file() {
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
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let session = LegacyPendingHydrationSession {
        lookup_records: Arc::new(vec![records[0].clone()]),
        hydration_records: Arc::new(records.clone()),
        hydration_error: None,
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let dir = TempDir::new().expect("temp dir");
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
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("filtered legacy pending file should be adopted");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let summary = db.get_summary().await.expect("summary");
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.awaiting_provider_verification, 0);
    let downloaded = db
        .get_downloaded_page(0, 10)
        .await
        .expect("downloaded page");
    assert_eq!(
        downloaded[0].local_path.as_deref(),
        Some(expected_path.as_path())
    );
}

#[tokio::test]
async fn bounded_full_sync_revalidates_policy_excluded_asset_after_later_deletion() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
    let record = TestAssetRecord::new("LEGACY_FILTERED_MISSING")
        .filename("legacy-filtered-missing.jpg")
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .size(1024)
        .added_at(Utc.timestamp_opt(1_700_000_000, 0).unwrap())
        .build();
    db.upsert_seen(&record).await.expect("seed pending row");
    db.backdate_last_seen(
        "LEGACY_FILTERED_MISSING",
        Utc::now().timestamp().saturating_sub(86_400),
    );

    let live_records =
        mock_photo_records_with_filename("LEGACY_FILTERED_MISSING", "legacy-filtered-missing.jpg");
    let session = LegacyPendingHydrationSession {
        lookup_records: Arc::new(vec![live_records[0].clone()]),
        hydration_records: Arc::new(live_records.clone()),
        hydration_error: None,
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let dir = TempDir::new().expect("temp dir");
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

    let present_passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(BoundedPolicyLookupSession {
                records: Arc::new(live_records),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
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

    let deleted_passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(BoundedPolicyLookupSession {
                records: Arc::new(vec![json!({
                    "recordName": "LEGACY_FILTERED_MISSING",
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                })]),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
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
    assert_eq!(deleted_result.sync_token, None);
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
    let session = MockPhotosFlow::new()
        .changes_zone_page(records, "zone-token-next", false)
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
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        Utc.timestamp_opt(1_746_994_800, 0).unwrap(),
    ));

    download_photos_with_sync(
        &Client::new(),
        &passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("bounded incremental sync should complete")
}

#[tokio::test]
async fn incremental_source_delete_prunes_pending_row_in_bounded_sync() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
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
        "source delete cleanup should not force full enumeration"
    );
    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(!result.stats.sync_token_blocked);
    assert!(db.get_pending().await.expect("pending page").is_empty());
}

#[tokio::test]
async fn incremental_prunes_existing_source_deleted_pending_row_without_wide_sync() {
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
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
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
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
}

#[tokio::test]
async fn metadata_backfill_in_another_library_does_not_force_full_enumeration() {
    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let row = TestAssetRecord::new("OTHER_LIBRARY_BACKFILL")
        .library("SharedSync-OTHER")
        .build();
    db.upsert_seen(&row).await.unwrap();
    db.mark_downloaded(
        "SharedSync-OTHER",
        "OTHER_LIBRARY_BACKFILL",
        "original",
        Path::new("/other.jpg"),
        "hash",
        None,
    )
    .await
    .unwrap();
    db.clear_metadata_hash_for_test("SharedSync-OTHER", "OTHER_LIBRARY_BACKFILL", "original");
    let result = run_date_bounded_incremental_sync(db.clone(), Vec::new()).await;
    assert!(!result.full_enumeration_ran);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(
        db.has_downloaded_without_metadata_hash("SharedSync-OTHER")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn hidden_invalid_capture_date_preserves_pending_recovery() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

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
                .expect(u64::from(!with_sibling))
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
                let db = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
                db.upsert_seen(&record).await.unwrap();
            }
            for cycle in 0..4 {
                eprintln!(
                    "pending date={invalid:?} sibling_hidden={sibling_hidden:?} cycle={cycle}: seed -> malformed/control -> reopen -> valid -> steady"
                );
                let recovered = !with_sibling && (valid_epoch || cycle >= 2);
                let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
                let mut records = incremental_photo_records_with_url(
                    "LEGACY_INVALID_CAPTURE",
                    "retained.jpg",
                    &format!("{}/retained.jpg", server.uri()),
                    8,
                );
                records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(checksum);
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
                let passes = vec![AlbumPass {
                    kind: PassKind::Unfiled,
                    album: album_with_session(
                        "PrimarySync",
                        "",
                        Box::new(LegacyPendingHydrationSession {
                            lookup_records: Arc::new(vec![records[0].clone()]),
                            hydration_records: Arc::new(records),
                            hydration_error: None,
                        }),
                    ),
                    exclude_ids: Arc::new(FxHashSet::default()),
                }];
                let mut config = test_config();
                config.directory = Arc::from(media.as_path());
                config.state_db = Some(db.clone());
                config.sync_mode = SyncMode::Full;
                config.recent = Some(300);
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
                let db = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
                let summary = db.get_summary().await.unwrap();
                assert_eq!(
                    summary.pending,
                    u64::from(!recovered),
                    "invalid={invalid:?}, sibling={with_sibling}, cycle={cycle}"
                );
                assert_eq!(summary.policy_excluded, 0);
                assert_eq!(summary.source_deleted, 0);
                assert_eq!(summary.downloaded, u64::from(recovered));
                // Retained pending debt does not veto current-source accounting.
                assert!(!result.checkpoint.identity_incomplete);
                assert_eq!(
                    result.stats.downloaded,
                    usize::from(!with_sibling && if valid_epoch { cycle == 0 } else { cycle == 2 })
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
                    assert_eq!(rows[0].created_at, record.created_at);
                    assert_eq!(rows[0].checksum.as_ref(), checksum);
                } else {
                    let rows = db.get_pending().await.unwrap();
                    assert_eq!(rows[0].created_at, record.created_at);
                    assert_eq!(rows[0].added_at, record.added_at);
                    assert_eq!(summary.awaiting_provider_verification, 1);
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

#[tokio::test]
async fn v027_targeted_mixed_renditions_keep_uncertainty_visible_after_reopen() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    #[derive(Clone)]
    struct SelectedRetrySession(PendingLookupSession);
    #[async_trait::async_trait]
    impl PhotosSession for SelectedRetrySession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({"batch":[{"records":[{"fields":{"itemCount":{"value":1}}}]}]}));
            }
            self.0.post(url, body, headers).await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    let server = crate::start_wiremock_or_skip!();
    let still = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let motion = include_bytes!("../../../../tests/data/media/pattern.mov");
    Mock::given(method("GET"))
        .and(path("/motion"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(motion.to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/still"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(still.to_vec()))
        .expect(0)
        .mount(&server)
        .await;
    let mut records = incremental_photo_records_with_url(
        "mixed",
        "mixed.JPG",
        &format!("{}/still", server.uri()),
        still.len() as u64,
    );
    records[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
        "downloadURL":format!("{}/motion",server.uri()),
        "size":motion.len(),"fileChecksum":data_encoding::BASE64.encode(&[0x72; 21])}});
    records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone())
        .with_state_record_name(Arc::from("asset-mixed"));
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let root = dir.path().join("media");
    let mut config = test_config();
    config.directory = Arc::from(root.as_path());
    config.folder_structure = "none".into();
    config.folder_structure_albums = Arc::from("{album}");
    let paths = filter::derive_expected_paths(&asset, &config);
    assert_eq!(paths.len(), 2);
    let still_path = paths
        .iter()
        .find(|p| p.version_size == VersionSizeKey::Original)
        .unwrap()
        .path
        .clone();
    std::fs::create_dir_all(still_path.parent().unwrap()).unwrap();
    let mut changed = still.to_vec();
    *changed.last_mut().unwrap() ^= 1;
    std::fs::write(&still_path, &changed).unwrap();
    let before_mtime = std::fs::metadata(&still_path).unwrap().modified().unwrap();
    {
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        for p in &paths {
            db.upsert_seen(
                &TestAssetRecord::new(asset.state_id())
                    .filename("mixed.JPG")
                    .checksum(&p.checksum)
                    .size(p.size)
                    .version_size(p.version_size)
                    .build(),
            )
            .await
            .unwrap();
        }
        db.upsert_asset_master_mapping("PrimarySync", asset.asset_record_name(), asset.id())
            .await
            .unwrap();
    }
    let mut passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session(
            "PrimarySync",
            "",
            Box::new(PendingLookupSession {
                records: Arc::new(records.clone()),
            }),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    // A later selected pass renders another missing destination. It must
    // not turn an uncertain rendition into a duplicate transfer.
    passes.push(AlbumPass {
        kind: PassKind::Album,
        album: album_with_session(
            "PrimarySync",
            "Other",
            Box::new(SelectedRetrySession(PendingLookupSession {
                records: Arc::new(records),
            })),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    });
    for cycle in 0..3 {
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        config.state_db = Some(db.clone());
        let result = super::run_targeted_recovery_pass(
            &Client::new(),
            &passes,
            &Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.stats.downloaded,
            usize::from(cycle == 0),
            "{result:?}"
        );
        assert!(
            matches!(
                result.outcome,
                DownloadOutcome::PartialFailure { failed_count: 1 }
            ),
            "{result:?}"
        );
        assert!(!result.checkpoint.sync_token_blocked);
        assert_eq!(result.checkpoint.state_write_failures, 0);
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].version_size, VersionSizeKey::Original);
        assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), 1);
        assert_eq!(std::fs::read(&still_path).unwrap(), changed);
        assert_eq!(
            std::fs::metadata(&still_path).unwrap().modified().unwrap(),
            before_mtime
        );
        config.state_db = None;
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
