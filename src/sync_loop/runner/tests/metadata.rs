use crate::sync_loop::test_support::MetadataSetFailure;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[cfg(feature = "xmp")]
use crate::config;
#[cfg(feature = "xmp")]
use crate::sync_cycle::preload_asset_groupings;
#[cfg(feature = "xmp")]
use crate::sync_cycle::run_cycle_with_witness;
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, run_cycle};
#[cfg(all(feature = "xmp", unix))]
use crate::sync_loop::test_support::make_named_empty_full_album;
#[cfg(feature = "xmp")]
use crate::sync_loop::test_support::make_named_full_album_with_boxed_session;
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions,
    album_count_response, full_album_page, full_album_page_with_download,
    make_full_album_with_boxed_session, make_full_album_with_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state_with_album, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle, make_state_db, media_without_photo_downloads,
    run_cycle_expected_date_dir,
};
use crate::{download, retry, state};

#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_capture_offset_drives_date_filter_path_and_sidecar() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let server = crate::start_wiremock_or_skip!();
    let body = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    Mock::given(method("GET"))
        .and(path("/capture-offset.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let capture_date = chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
    let mut page = full_album_page_with_download(
        "PrimarySync",
        "capture-offset",
        "zone-token",
        &format!("{}/capture-offset.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    page["records"][1]["fields"]["assetDate"]["value"] = serde_json::json!(1_769_898_719_629_i64);
    page["records"][1]["fields"]["addedDate"]["value"] = serde_json::json!(1_769_898_719_789_i64);
    page["records"][1]["fields"]["timeZoneOffset"] =
        serde_json::json!({"value": 39_600, "type": "INT64"});

    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(page),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let boundary = config::CreatedDateFilter::CaptureDate(capture_date);
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            skip_created_before: Some(boundary),
            skip_created_after: Some(boundary),
            xmp_sidecar: true,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let mut config = make_run_cycle_config();
    config.filters.skip_created_before = Some(boundary);
    config.filters.skip_created_after = Some(boundary);
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
    .expect("run capture-offset cycle");

    assert_eq!(result.stats.downloaded, 1);
    let downloaded = db.get_downloaded_page(0, 1).await.expect("downloaded row");
    assert_eq!(
        downloaded[0].created_at.timestamp_millis(),
        1_769_898_719_629
    );
    assert_eq!(
        downloaded[0].added_at.map(|date| date.timestamp_millis()),
        Some(1_769_898_719_789)
    );
    let media_path = downloaded[0].local_path.as_ref().expect("downloaded path");
    assert!(
        media_path
            .parent()
            .is_some_and(|parent| parent.ends_with("2026/02/01"))
    );
    assert_eq!(std::fs::read(media_path).expect("downloaded media"), body);

    let sidecar = std::fs::read_to_string(media_path.with_file_name(format!(
            "{}.xmp",
            media_path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("media filename")
        )))
    .expect("capture-offset sidecar");
    let metadata = sidecar.parse::<XmpMeta>().expect("valid XMP sidecar");
    for (namespace, property) in [
        (xmp_ns::XMP, "CreateDate"),
        (xmp_ns::XMP, "ModifyDate"),
        (xmp_ns::EXIF, "DateTimeOriginal"),
        (xmp_ns::PHOTOSHOP, "DateCreated"),
    ] {
        assert_eq!(
            metadata
                .property(namespace, property)
                .expect(property)
                .value,
            "2026-02-01T09:31:59.629+11:00"
        );
    }
    assert_eq!(
        metadata
            .property("http://cipa.jp/exif/1.0/", "OffsetTimeOriginal")
            .expect("OffsetTimeOriginal")
            .value,
        "+11:00"
    );
}

#[tokio::test]
async fn incremental_invalid_capture_date_preserves_prior_checkpoint() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .expect("seed current enum config hash");

    let mut page = full_album_page("PrimarySync", "bad-date", "zone-tok-new");
    page["records"][1]["fields"]["assetDate"]["value"] = serde_json::json!("not-a-timestamp");
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "zone-tok-new",
                "moreComing": false,
                "records": page["records"].clone()
            }]
        })),
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
    .expect("run malformed capture-date cycle");

    assert_eq!(result.failed_count, 0);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(crate::icloud::photos::asset::MALFORMED_REQUIRED_ASSET_FIELDS_REASON)
    );
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(result.stats.skipped.total(), 0);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "invalid capture dates must replay from the prior checkpoint"
    );
}

// ── preload_asset_groupings ──────────────────────────────────────
//
// `preload_asset_groupings` must be best-effort: a hiccup
// loading people must NOT empty the albums map, and vice versa.
// XMP-sidecar runs read this struct; biasing the entire grouping
// empty would silently strip metadata from every downloaded photo.

/// When `get_all_asset_albums` succeeds but
/// `get_all_asset_people` fails, the result still includes albums.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn preload_asset_groupings_partial_people_failure_keeps_albums() {
    struct PartialDb {
        inner: Arc<dyn download::DownloadStore>,
    }

    #[async_trait::async_trait]
    impl state::MembershipStore for PartialDb {
        async fn add_asset_album(
            &self,
            library: &str,
            asset_id: &str,
            album_name: &str,
            source: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .add_asset_album(library, asset_id, album_name, source)
                .await
        }

        async fn get_all_asset_albums(
            &self,
            library: &str,
        ) -> Result<Vec<(String, String)>, state::error::StateError> {
            self.inner.get_all_asset_albums(library).await
        }

        async fn get_all_asset_people(
            &self,
            _: &str,
        ) -> Result<Vec<(String, String)>, state::error::StateError> {
            Err(state::error::StateError::LockPoisoned(
                "simulated people-table read failure".into(),
            ))
        }
    }

    // Seed the inner DB with two album memberships across two assets,
    // so we can verify the surviving map is non-empty.
    let inner = make_state_db();
    inner
        .add_asset_album("PrimarySync", "ASSET_A", "Vacation", "icloud")
        .await
        .expect("add album A");
    inner
        .add_asset_album("PrimarySync", "ASSET_B", "Family", "icloud")
        .await
        .expect("add album B");

    let db = PartialDb { inner };

    let groupings = preload_asset_groupings(Some(&db), "PrimarySync").await;
    // Albums must survive intact.
    assert_eq!(
        groupings.albums.len(),
        2,
        "two assets with album memberships expected, got {}",
        groupings.albums.len()
    );
    assert!(groupings.albums.contains_key("ASSET_A"));
    assert!(groupings.albums.contains_key("ASSET_B"));
    // People map is empty (the read failed) — but the function still
    // returns Some groupings rather than panicking.
    assert!(
        groupings.people.is_empty(),
        "people map should be empty when its read failed; got {} entries",
        groupings.people.len()
    );
}

/// Companion: `state_db = None` returns an empty grouping struct.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn preload_asset_groupings_no_db_returns_empty() {
    let groupings = preload_asset_groupings::<state::SqliteStateDb>(None, "PrimarySync").await;
    assert!(groupings.albums.is_empty());
    assert!(groupings.people.is_empty());
}

#[tokio::test]
async fn run_cycle_provider_metadata_write_failure_preserves_zone_checkpoint() {
    let mut config = make_run_cycle_config();
    config.filters.recent = Some(10);
    let inner = make_state_db();
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(
            Arc::clone(&inner),
            "simulated provider metadata write failure",
        )
        .with_refresh_downloaded_metadata_failure(),
    );
    let page = run_cycle_favourited_asset_page();
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "zone-tok-new",
                "moreComing": false,
                "records": page["records"].clone()
            }]
        })).ok(serde_json::json!({"records":page["records"].clone(),"syncToken":"metadata-rank-token"})),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            recent: Some(10),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

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

    assert!(result.failed_count > 0);
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.failed, 0);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some("provider_metadata_state_write_failed")
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "failed provider metadata write must preserve the replay checkpoint"
    );
}

/// Seeds a downloaded asset whose stored metadata is not a favourite,
/// with its media file on disk, ready for the provider to report a
/// metadata-only edit against it.
async fn seed_run_cycle_metadata_drift_asset(
    inner: &Arc<dyn download::DownloadStore>,
    download_dir: &std::path::Path,
) {
    use chrono::TimeZone as _;

    let media_dir = download_dir.join(run_cycle_expected_date_dir());
    std::fs::create_dir_all(&media_dir).expect("create media directory");
    let media_path = media_dir.join("photo.jpg");
    std::fs::write(&media_path, vec![0u8; 1024]).expect("seed media file");

    let mut stored_metadata = state::AssetMetadata {
        is_favorite: false,
        ..state::AssetMetadata::default()
    };
    stored_metadata.refresh_hash();
    let record = crate::test_helpers::TestAssetRecord::new("master-PrimarySync")
        .filename("photo.jpg")
        .added_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
        .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        .created_at(
            chrono::Utc
                .timestamp_millis_opt(RUN_CYCLE_ASSET_DATE_MS)
                .single()
                .expect("valid asset date"),
        )
        .size(1024)
        .metadata(stored_metadata)
        .build();
    inner.upsert_seen(&record).await.expect("seed state row");
    inner
        .mark_downloaded(
            "PrimarySync",
            "master-PrimarySync",
            "original",
            &media_path,
            "local-checksum",
            None,
        )
        .await
        .expect("mark downloaded");
}

/// The same asset as `seed_run_cycle_metadata_drift_asset`, but reported
/// by the provider as a favourite, so its metadata drifts from the row.
fn run_cycle_favourited_asset_page() -> serde_json::Value {
    let mut page = full_album_page_with_download(
        "PrimarySync",
        "master-PrimarySync",
        "zone-tok-new",
        "https://p01.icloud-content.com/photo.jpg",
        1024,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    );
    page["records"][1]["fields"]["isFavorite"] = serde_json::json!({"value": 1, "type": "INT64"});
    page
}

/// The same asset carrying a caption edit instead of a favourite, so two
/// passes can deliver different provider snapshots of one asset and both
/// drift from the stored row.
#[cfg(feature = "xmp")]
fn run_cycle_captioned_asset_page() -> serde_json::Value {
    let mut page = run_cycle_favourited_asset_page();
    page["records"][1]["fields"]["isFavorite"] = serde_json::json!({"value": 0, "type": "INT64"});
    page["records"][1]["fields"]["captionEnc"] =
        serde_json::json!({"value": "edited in another pass", "type": "STRING"});
    page
}

// Current ranked inventories complement sparse source/lookup fixtures. Rank
// EOF is deliberately distinct from the source successor and never clears debt.
fn sparse_fixture_inventory(
    url: &str,
    request: &serde_json::Value,
    records: &[serde_json::Value],
) -> Option<serde_json::Value> {
    if url.contains("/internal/records/query/batch?") {
        return Some(album_count_response((records.len() / 2) as u64));
    }
    if !url.contains("/records/query?") {
        return None;
    }
    let offset = request["query"]["filterBy"]
        .as_array()?
        .iter()
        .find(|filter| filter["fieldName"] == "startRank")
        .and_then(|filter| filter["fieldValue"]["value"].as_u64())
        .unwrap_or(0);
    Some(
        serde_json::json!({"records": if offset == 0 {records} else {&[]},
        "syncToken":"sparse-fixture-rank"}),
    )
}

#[tokio::test]
#[allow(
    clippy::print_stderr,
    reason = "synthetic provider and state measurements are test evidence"
)]
async fn unresolved_identity_survives_restart_and_other_zone_success_then_recovers() {
    use crate::state::SparseIdentityStore as _;
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum EvidenceCase {
        Ordinary,
        Sparse,
        DeletedDelta,
        HardDeletedDelta,
        Malformed,
        Changed,
        MarkerWriteFailure,
    }

    #[derive(Clone, Debug)]
    struct IdentitySession {
        unresolved: bool,
        lookups: Arc<std::sync::atomic::AtomicUsize>,
        requests: Arc<std::sync::Mutex<Vec<String>>>,
        pages: Arc<std::sync::Mutex<Vec<(u64, u64, usize)>>>,
        evidence: EvidenceCase,
        records: Vec<serde_json::Value>,
        valid: Vec<serde_json::Value>,
        recovered: Vec<serde_json::Value>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for IdentitySession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let kind = if request["query"]["recordType"] == "CheckIndexingState" {
                "index"
            } else if url.contains("/changes/zone?") {
                "delta"
            } else if url.contains("/records/lookup?") {
                let names = request["records"].as_array().unwrap();
                let has_child = names.iter().any(|item| {
                    item["recordName"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("asset-valid-new"))
                });
                let has_master = names.iter().any(|item| {
                    item["recordName"]
                        .as_str()
                        .is_some_and(|name| name.starts_with("valid-new"))
                });
                if names
                    .iter()
                    .any(|item| item["recordName"] == "unresolved-child")
                {
                    "source_lookup"
                } else if names.len() == 1 && has_child {
                    "child_lookup"
                } else if names.len() == 2 && has_child && has_master {
                    "pair_lookup"
                } else {
                    "other_lookup"
                }
            } else if url.contains("/records/query?")
                && request["query"]["filterBy"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["fieldName"] == "startRank"))
            {
                "rank"
            } else if url.contains("/internal/records/query/batch?") {
                "count"
            } else {
                "other"
            };
            self.requests.lock().unwrap().push(kind.to_owned());
            if request["query"]["recordType"] == "CheckIndexingState" {
                return Ok(
                    serde_json::json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
                );
            }
            if kind == "rank" {
                let rank = request["query"]["filterBy"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|filter| filter["fieldName"] == "startRank")
                    .unwrap()["fieldValue"]["value"]
                    .as_u64()
                    .unwrap();
                let limit = request["resultsLimit"].as_u64().unwrap();
                let start = usize::try_from(rank)
                    .unwrap()
                    .saturating_mul(2)
                    .min(self.valid.len());
                let end = start
                    .saturating_add(usize::try_from(limit).unwrap())
                    .min(self.valid.len());
                let records = &self.valid[start..end];
                self.pages
                    .lock()
                    .unwrap()
                    .push((rank, limit, records.len()));
                return Ok(
                    serde_json::json!({"records": records, "syncToken": "sparse-fixture-rank"}),
                );
            }
            if let Some(page) = sparse_fixture_inventory(url, &request, &self.valid) {
                return Ok(page);
            }
            if url.contains("/records/lookup?") {
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(
                    body["zoneID"],
                    serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})
                );
                let requested = body["records"].as_array().unwrap();
                let is_source = requested
                    .iter()
                    .any(|item| item["recordName"] == "unresolved-child");
                if is_source {
                    self.lookups
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let records = if self.unresolved && is_source {
                    if self.evidence == EvidenceCase::Ordinary {
                        Vec::new()
                    } else {
                        let mut record = crate::test_helpers::sparse_shared_asset_record();
                        record["recordName"] = serde_json::json!("unresolved-child");
                        match self.evidence {
                            EvidenceCase::Malformed => {
                                record["fields"]["linkedShareZoneOwner"]["value"] =
                                    serde_json::json!(null)
                            }
                            EvidenceCase::Changed => {
                                record["fields"]["linkedShareRecordName"]["value"] =
                                    serde_json::json!("different-private-child");
                                record["fields"]["linkedShareZoneName"]["value"] =
                                    serde_json::json!("SharedSync-different-private-zone");
                                record["fields"]["linkedShareZoneOwner"]["value"]["recordName"] =
                                    serde_json::json!("different-private-owner");
                            }
                            _ => {}
                        }
                        vec![record]
                    }
                } else {
                    self.valid
                        .iter()
                        .chain(self.recovered.iter())
                        .filter(|record| {
                            requested
                                .iter()
                                .any(|item| item["recordName"] == record["recordName"])
                        })
                        .cloned()
                        .collect()
                };
                return Ok(serde_json::json!({"records":records}));
            }
            if url.contains("/changes/zone?") {
                if request["zones"][0]["syncToken"] == "zone-after" {
                    return Ok(
                        serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"zone-after","moreComing":false,"records":[]}]}),
                    );
                }
                assert_eq!(request["zones"][0]["syncToken"], "zone-before");
                let mut child = if self.evidence != EvidenceCase::Ordinary {
                    crate::test_helpers::sparse_shared_asset_record()
                } else {
                    self.records[1].clone()
                };
                if self.evidence == EvidenceCase::Malformed {
                    child["fields"]["linkedShareZoneOwner"]["value"] = serde_json::json!(null);
                }
                child["recordName"] = serde_json::json!("unresolved-child");
                child["fields"].as_object_mut().unwrap().remove("masterRef");
                if self.evidence == EvidenceCase::DeletedDelta && !self.unresolved {
                    child["fields"]["isDeleted"] = serde_json::json!({"value":1,"type":"INT64"});
                }
                if self.evidence == EvidenceCase::HardDeletedDelta && !self.unresolved {
                    child["deleted"] = serde_json::json!(true);
                }
                let mut records = self.valid.clone();
                if self.unresolved
                    || matches!(
                        self.evidence,
                        EvidenceCase::DeletedDelta | EvidenceCase::HardDeletedDelta
                    )
                {
                    records.push(child);
                } else {
                    records.extend(self.recovered.clone());
                }
                return Ok(serde_json::json!({"zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-after", "moreComing": false, "records": records
                }]}));
            }
            panic!("identity replay must not fall back to full enumeration: {url}");
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    let snapshot_files = |directory: &std::path::Path| {
        let mut files = std::collections::BTreeMap::new();
        let mut directories = vec![directory.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    directories.push(path);
                } else {
                    files.insert(path.clone(), std::fs::read(path).unwrap());
                }
            }
        }
        files
    };

    let json_rows = |conn: &rusqlite::Connection, sql: &str| -> Vec<serde_json::Value> {
        let mut statement = conn.prepare(sql).unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
            .collect()
    };

    let legacy_snapshot = |database: &std::path::Path| {
        let conn = rusqlite::Connection::open(database).unwrap();
        [
            "SELECT * FROM unattributed_legacy WHERE library='PrimarySync' AND asset_id='legacy-family'",
            "SELECT * FROM unattributed_legacy_paths WHERE library='PrimarySync' AND asset_id='legacy-family' ORDER BY path_key",
            "SELECT * FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id='legacy-family'",
            "SELECT * FROM unattributed_legacy_proofs WHERE library='PrimarySync' AND asset_id='legacy-family'",
        ].map(|sql| {
            let mut stmt = conn.prepare(sql).unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |row| (0..columns).map(|i| row.get::<_, rusqlite::types::Value>(i)).collect::<Result<Vec<_>, _>>()).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        })
    };

    for (recent, evidence, size) in [Some(10), None].into_iter().flat_map(|recent| {
        [
            EvidenceCase::Ordinary,
            EvidenceCase::Sparse,
            EvidenceCase::DeletedDelta,
            EvidenceCase::HardDeletedDelta,
            EvidenceCase::Malformed,
            EvidenceCase::Changed,
            EvidenceCase::MarkerWriteFailure,
        ]
        .into_iter()
        .flat_map(move |evidence| {
            let sizes: &[usize] = if recent.is_none() && evidence == EvidenceCase::Sparse {
                &[1, 16]
            } else {
                &[1]
            };
            sizes
                .iter()
                .copied()
                .map(move |size| (recent, evidence, size))
        })
    }) {
        let server = crate::start_wiremock_or_skip!();
        // Valid JPEG framing; no optional metadata writer is needed.
        let bytes = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));
        Mock::given(method("GET"))
            .and(path("/valid.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .expect(
                u64::try_from(
                    size + usize::from(
                        recent.is_none()
                            && !matches!(
                                evidence,
                                EvidenceCase::DeletedDelta | EvidenceCase::HardDeletedDelta
                            ),
                    ),
                )
                .unwrap(),
            )
            .mount(&server)
            .await;
        let valid: Vec<_> = (0..size)
            .flat_map(|i| {
                let id = if size == 1 {
                    "valid-new".to_owned()
                } else {
                    format!("valid-new-{i}")
                };
                let mut page = full_album_page_with_download(
                    "PrimarySync",
                    &id,
                    "zone-after",
                    &format!("{}/valid.jpg", server.uri()),
                    bytes.len() as u64,
                    &checksum,
                );
                if size > 1 {
                    page["records"][0]["fields"]["filenameEnc"]["value"] =
                        serde_json::json!(format!("unique-{i}.jpg"));
                }
                page["records"].as_array().unwrap().clone()
            })
            .collect();
        let mut recovered = full_album_page_with_download(
            "PrimarySync",
            "recovered",
            "zone-after",
            &format!("{}/valid.jpg", server.uri()),
            bytes.len() as u64,
            &checksum,
        )["records"]
            .as_array()
            .unwrap()
            .clone();
        recovered[1]["recordName"] = serde_json::json!("unresolved-child");
        let owner = crate::state::db::account::AccountOwner::authenticated(
            "identity@example.invalid",
            "com",
            &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-identity"}}))
                .unwrap(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("state.db");
        let media = dir.path().join("media");
        {
            let inner = Arc::new(
                state::SqliteStateDb::open_owned(&database, &owner)
                    .await
                    .unwrap(),
            );
            let db = inner.clone() as Arc<dyn download::DownloadStore>;
            seed_run_cycle_metadata_drift_asset(&db, &media).await;
            let run = inner.start_sync_run().await.unwrap();
            inner
                .complete_sync_run(run, &state::SyncRunStats::default())
                .await
                .unwrap();
            inner.set_metadata_capture_revision_for_test(
                "PrimarySync",
                "master-PrimarySync",
                state::METADATA_CAPTURE_REVISION,
            );
            inner
                .upsert_asset_master_mapping(
                    "PrimarySync",
                    "private-linked-child",
                    "master-PrimarySync",
                )
                .await
                .unwrap();
            inner
                .upsert_asset_master_mapping(
                    "PrimarySync",
                    "different-private-child",
                    "master-PrimarySync",
                )
                .await
                .unwrap();
            if evidence == EvidenceCase::Sparse {
                // Separate ambiguous historical family, prepared by its owner;
                // no current source receipt or child ownership is invented.
                let path = media.join("legacy-family.jpg");
                let original = b"synthetic historical bytes";
                std::fs::write(&path, original).unwrap();
                let provider_checksum =
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(original));
                let local_hash = download::file::compute_sha256(&path).await.unwrap();
                let row = crate::test_helpers::TestAssetRecord::new("legacy-family")
                    .library("PrimarySync")
                    .filename("legacy-family.jpg")
                    .size(original.len() as u64)
                    .checksum(&provider_checksum)
                    .build();
                inner.upsert_seen(&row).await.unwrap();
                inner
                    .mark_downloaded(
                        "PrimarySync",
                        "legacy-family",
                        "original",
                        &path,
                        &local_hash,
                        None,
                    )
                    .await
                    .unwrap();
                for child in ["legacy-child-a", "legacy-child-b"] {
                    inner
                        .upsert_asset_master_mapping("PrimarySync", child, "legacy-family")
                        .await
                        .unwrap();
                }
                inner.set_metadata_capture_revision_for_test("PrimarySync", "legacy-family", 0);
                inner
                    .begin_metadata_capture_revision(
                        "PrimarySync",
                        state::METADATA_CAPTURE_REVISION,
                    )
                    .await
                    .unwrap();
                let candidate = inner
                    .get_metadata_capture_candidates(
                        "PrimarySync",
                        state::METADATA_CAPTURE_REVISION,
                        10,
                    )
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|item| item.asset_id == "legacy-family")
                    .unwrap();
                assert!(
                    inner
                        .defer_metadata_capture_ambiguity(
                            &candidate,
                            state::METADATA_CAPTURE_REVISION
                        )
                        .await
                        .unwrap()
                );
                let snapshot = inner
                    .legacy_preparation_snapshots("PrimarySync", 10)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|item| item.asset_id == "legacy-family")
                    .unwrap();
                let sidecar = media.join("legacy-family.jpg.xmp");
                assert!(!sidecar.exists());
                let files = [
                    state::LegacyFileEvidence {
                        root: media.clone(),
                        path: path.clone(),
                        sha256: Some(local_hash),
                        size: Some(original.len() as u64),
                    },
                    state::LegacyFileEvidence {
                        root: media.clone(),
                        path: sidecar,
                        sha256: None,
                        size: None,
                    },
                ];
                assert!(
                    inner
                        .prepare_legacy_preservation(&snapshot, &files)
                        .await
                        .unwrap()
                );
            }
            for zone in ["PrimarySync", "SharedSync-test"] {
                inner
                    .set_metadata(&crate::sync_cycle::sync_token_key(zone), "zone-before")
                    .await
                    .unwrap();
            }
        }
        let before = snapshot_files(&media);
        let recovered_path = media
            .join(run_cycle_expected_date_dir())
            .join(if size == 1 {
                "cGhvdG8uanBn-unresolved-child.JPG"
            } else {
                "cGhvdG8uanBn.JPG"
            });
        let legacy_before = (evidence == EvidenceCase::Sparse).then(|| legacy_snapshot(&database));
        if let Some(rows) = &legacy_before {
            assert_eq!(
                rows.each_ref().map(Vec::len),
                [1, 2, 1, 0],
                "one prepared family and its separate retry must exist without attribution proof"
            );
        }
        let mut after_download = None;
        let mut selected_receipts = None;
        let mut config = make_run_cycle_config();
        config.filters.recent = recent;
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        // Reopen SQLite each time. After marker failure, replay once to persist it
        // before selecting only the clean zone, recovering, and checking steady state.
        let marker_failure = evidence == EvidenceCase::MarkerWriteFailure;
        let deletion_delta = matches!(
            evidence,
            EvidenceCase::DeletedDelta | EvidenceCase::HardDeletedDelta
        );
        let recovery_phase = 3 + usize::from(deletion_delta);
        let extra_quiet = evidence == EvidenceCase::Sparse;
        let mut quiet_counts = None;
        let mut quiet_debt = None;
        let mut quiet_state = None;
        let mut recovered_receipts_snapshot = None;
        let mut recovered_destinations_snapshot = None;
        let mut expected_media_gets = 0usize;
        for cycle in 0..(6
            + usize::from(marker_failure)
            + usize::from(deletion_delta)
            + usize::from(extra_quiet))
        {
            let phase = cycle
                .saturating_sub(usize::from(marker_failure))
                .saturating_sub(usize::from(extra_quiet && cycle >= 3));
            let fail_marker = marker_failure && cycle == 0;
            let fail_deletion = deletion_delta && phase == 3;
            let recovery_publication =
                phase == recovery_phase && recent.is_none() && !deletion_delta;
            let inner = Arc::new(
                state::SqliteStateDb::open_owned(&database, &owner)
                    .await
                    .unwrap(),
            );
            if phase == 3 {
                // Simulate the retry deadline without sleeping or changing production time.
                rusqlite::Connection::open(&database).unwrap().execute(
                    "UPDATE unresolved_sparse_identities SET next_retry_at=0 WHERE library='PrimarySync'", []
                ).unwrap();
            }
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let pages = Arc::new(std::sync::Mutex::new(Vec::new()));
            let db: Arc<dyn download::DownloadStore> = if fail_marker {
                Arc::new(FailingMetadataSetDb::new(
                    inner.clone(),
                    MetadataSetFailure::Prefix(state::UNRESOLVED_IDENTITY_PREFIX),
                    "injected unresolved marker write failure",
                ))
            } else if fail_deletion {
                let mut failing = FailingMetadataSetDb::without_set_failure(
                    inner.clone(),
                    "injected source deletion write failure",
                );
                failing.fail_source_delete = true;
                Arc::new(failing)
            } else {
                inner.clone()
            };
            let mut primary_library = crate::icloud::photos::PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(IdentitySession {
                    unresolved: phase < 3,
                    lookups: lookups.clone(),
                    requests: requests.clone(),
                    pages: pages.clone(),
                    evidence,
                    records: run_cycle_favourited_asset_page()["records"]
                        .as_array()
                        .unwrap()
                        .clone(),
                    valid: valid.clone(),
                    recovered: recovered.clone(),
                }),
                Arc::new(
                    serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            // Fault wrappers are not the concrete capture database. The next
            // normal cycle restores scoped capture and replays the held source.
            if !fail_marker && !fail_deletion {
                primary_library.shadow_capture =
                    Some(crate::icloud::photos::inbox::ShadowCapture::new(
                        inner.clone(),
                        owner.clone(),
                        "com",
                    ));
            }
            let mut primary = make_run_cycle_library_state_with_album(
                "PrimarySync",
                &crate::sync_cycle::sync_token_key("PrimarySync"),
                primary_library.all(),
            );
            primary.library = primary_library;
            #[derive(Clone)]
            struct QuietShared;
            #[async_trait::async_trait]
            impl crate::icloud::photos::PhotosSession for QuietShared {
                async fn post(
                    &self,
                    url: &str,
                    body: String,
                    _: &[(&str, &str)],
                ) -> anyhow::Result<serde_json::Value> {
                    let request: serde_json::Value = serde_json::from_str(&body)?;
                    if request["query"]["recordType"] == "CheckIndexingState" {
                        return Ok(
                            serde_json::json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
                        );
                    }
                    if let Some(page) = sparse_fixture_inventory(url, &request, &[]) {
                        return Ok(page);
                    }
                    assert!(url.contains("/changes/zone?"));
                    assert_eq!(
                        request["zones"][0]["zoneID"],
                        serde_json::json!({"zoneName":"SharedSync-test","ownerRecordName":"_defaultOwner"})
                    );
                    Ok(
                        serde_json::json!({"zones":[{"zoneID":{"zoneName":"SharedSync-test","ownerRecordName":"_defaultOwner"},"records":[],"syncToken":"shared-after","moreComing":false}]}),
                    )
                }
                fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
                    Box::new(self.clone())
                }
            }
            let mut shared_library = crate::icloud::photos::PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()), Box::new(QuietShared),
                Arc::new(serde_json::json!({"zoneName":"SharedSync-test","ownerRecordName":"_defaultOwner"})),
                "private".into(), crate::retry::RetryConfig::default(),
            ).await.unwrap();
            // Fault wrappers are not the concrete capture database. The next
            // normal cycle restores scoped capture and replays the held source.
            if !fail_marker && !fail_deletion {
                shared_library.shadow_capture =
                    Some(crate::icloud::photos::inbox::ShadowCapture::new(
                        inner.clone(),
                        owner.clone(),
                        "com",
                    ));
            }
            let mut shared = make_run_cycle_library_state_with_album(
                "SharedSync-test",
                &crate::sync_cycle::sync_token_key("SharedSync-test"),
                shared_library.all(),
            );
            shared.library = shared_library;
            let libraries = if phase == 1 {
                vec![&shared]
            } else {
                vec![&primary, &shared]
            };
            let builder = make_run_cycle_download_config_builder_with_options(
                &media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    recent,
                    ..RunCycleDownloadConfigOptions::default()
                },
            );
            let result = run_cycle(
                &libraries,
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
            let selected_identity_incomplete = phase != 1 && phase < recovery_phase;
            assert_eq!(
                result.stats.identity_incomplete, selected_identity_incomplete,
                "recent={recent:?} evidence={evidence:?} cycle={cycle}"
            );
            assert_eq!(result.failed_count > 0, selected_identity_incomplete);
            let deferred = phase == 2
                && matches!(
                    evidence,
                    EvidenceCase::Sparse
                        | EvidenceCase::DeletedDelta
                        | EvidenceCase::HardDeletedDelta
                        | EvidenceCase::MarkerWriteFailure
                );
            assert_eq!(
                lookups.load(std::sync::atomic::Ordering::Relaxed),
                if phase == 3 && !deletion_delta {
                    if recent.is_some() { 2 } else { 4 }
                } else {
                    usize::from(phase != 1 && !deferred && phase < 3)
                },
                "recent={recent:?} evidence={evidence:?} cycle={cycle} phase={phase}"
            );
            if extra_quiet && phase == 2 {
                let counts = {
                    let requests = requests.lock().unwrap();
                    [
                        "index",
                        "rank",
                        "count",
                        "delta",
                        "child_lookup",
                        "pair_lookup",
                        "source_lookup",
                        "other_lookup",
                        "other",
                    ]
                    .map(|kind| requests.iter().filter(|item| item.as_str() == kind).count())
                };
                let debt = {
                    let conn = inner
                        .acquire_lock("snapshot unchanged sparse debt")
                        .unwrap();
                    let mut stmt = conn.prepare("SELECT * FROM unresolved_sparse_identities ORDER BY library, source_record_name").unwrap();
                    let columns = stmt.column_count();
                    stmt.query_map([], |row| {
                        (0..columns)
                            .map(|i| row.get::<_, rusqlite::types::Value>(i))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
                };
                assert_eq!(debt.len(), 1);
                if let Some(prior) = &quiet_counts {
                    assert_eq!(
                        &counts, prior,
                        "unchanged selected follow-ups must have stable request counts"
                    );
                } else {
                    quiet_counts = Some(counts);
                }
                if let Some(prior) = &quiet_debt {
                    assert_eq!(
                        &debt, prior,
                        "unchanged follow-ups must preserve exact sparse attribution"
                    );
                } else {
                    quiet_debt = Some(debt);
                }
                let state_counts = {
                    let conn = inner.acquire_lock("measure quiet retained state").unwrap();
                    [
                        "assets",
                        "provider_active_generations",
                        "provider_active_decisions",
                        "provider_active_destinations",
                        "provider_shadow_pages",
                        "unresolved_sparse_identities",
                    ]
                    .map(|table| {
                        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                            row.get::<_, i64>(0)
                        })
                        .unwrap()
                    })
                };
                if let Some(prior) = quiet_state {
                    assert_eq!(
                        state_counts, prior,
                        "unchanged selected cycles must not accumulate retained rows"
                    );
                } else {
                    quiet_state = Some(state_counts);
                }
                let media_gets = server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|request| request.url.path() == "/valid.jpg")
                    .count();
                let rank_pages = pages.lock().unwrap().clone();
                assert_eq!(
                    counts,
                    [
                        3,
                        if recent.is_some() { 6 } else { 0 },
                        0,
                        1,
                        size,
                        size,
                        0,
                        0,
                        0
                    ],
                    "classify provider requests by actual query and lookup members"
                );
                assert_eq!(
                    media_gets, size,
                    "no repeated media GET after the first cycle"
                );
                if recent.is_some() {
                    assert_eq!(
                        rank_pages,
                        [
                            (0, 4, 2),
                            (1, 4, 0),
                            (3, 4, 0),
                            (5, 4, 0),
                            (7, 4, 0),
                            (9, 4, 0)
                        ]
                    );
                } else {
                    assert!(rank_pages.is_empty());
                }
                eprintln!(
                    "SPARSE_QUIET_REACHED recent={recent:?} size={size} cycle={cycle} index={} rank={} count={} delta={} child_lookup={} pair_lookup={} source_lookup={} other_lookup={} other={} media_gets_total={media_gets} rank_pages={rank_pages:?} state_rows={state_counts:?}",
                    counts[0],
                    counts[1],
                    counts[2],
                    counts[3],
                    counts[4],
                    counts[5],
                    counts[6],
                    counts[7],
                    counts[8]
                );
            }
            let retained = inner.sparse_identities("PrimarySync").await.unwrap();
            assert_eq!(
                retained.len(),
                usize::from(phase < recovery_phase && !fail_marker),
                "recent={recent:?} evidence={evidence:?} cycle={cycle}"
            );
            if deferred {
                assert!(retained[0].next_retry.is_some());
            }
            let selected_legacy_pending = evidence == EvidenceCase::Sparse && phase != 1;
            assert_eq!(
                result.stats.unattributed_legacy_pending,
                u64::from(selected_legacy_pending),
                "legacy status is independent of sparse identity recovery"
            );
            assert_eq!(
                crate::cycle_reporter::classify_cycle(
                    &result.stats,
                    result.failed_count,
                    result.session_expired
                ),
                if selected_identity_incomplete || selected_legacy_pending {
                    crate::cycle_reporter::CycleStatus::Failed
                } else {
                    crate::cycle_reporter::CycleStatus::Success
                }
            );
            assert_eq!(
                result.stats.state_write_failures > 0,
                fail_marker || fail_deletion
            );
            assert_eq!(
                inner
                    .get_master_record_name_for_asset("PrimarySync", "unresolved-child")
                    .await
                    .unwrap()
                    .as_deref(),
                (phase >= recovery_phase && !deletion_delta).then_some("recovered"),
                "recent={recent:?} evidence={evidence:?} cycle={cycle}"
            );
            for child in ["private-linked-child", "different-private-child"] {
                assert_eq!(
                    inner
                        .get_master_record_name_for_asset("PrimarySync", child)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("master-PrimarySync"),
                    "recent={recent:?} evidence={evidence:?} cycle={cycle}"
                );
            }
            let marker_expected = phase < recovery_phase && !fail_marker;
            assert_eq!(
                inner
                    .get_metadata(&state::unresolved_identity_key("PrimarySync"))
                    .await
                    .unwrap()
                    .as_deref(),
                marker_expected.then_some("1")
            );
            if evidence == EvidenceCase::Sparse && phase <= 2 {
                let (account_key, provider_key) = owner.authenticated_keys().unwrap();
                let zone =
                    serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
                let scope = crate::icloud::photos::inbox::ShadowCapture::new(
                    inner.clone(),
                    owner.clone(),
                    "com",
                )
                .scope("private", &zone)
                .unwrap();
                let pass = &primary.plan.passes[0];
                let expected_pass = format!("{:x}", Sha256::digest(serde_json::json!({"kind": format!("{:?}", pass.kind), "scope": pass.album.selection_scope(), "excludes": Vec::<String>::new()}).to_string().as_bytes()));
                let content_hash = format!("{:x}", Sha256::digest(&bytes));
                let files = snapshot_files(&media);
                let one_path = if size == 1 {
                    let candidates: Vec<_> = files
                        .iter()
                        .filter(|(_, content)| *content == &bytes)
                        .map(|(path, _)| path.clone())
                        .collect();
                    assert_eq!(
                        candidates.len(),
                        1,
                        "one-asset case has exactly one independently observed selected file"
                    );
                    Some(candidates[0].clone())
                } else {
                    None
                };
                let conn = inner
                    .acquire_lock("inspect exact selected receipts")
                    .unwrap();
                let mut receipts = Vec::new();
                let receipt_count: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE library='PrimarySync' AND child LIKE 'asset-valid-new%'", [], |row| row.get(0)).unwrap();
                assert_eq!(receipt_count, i64::try_from(size).unwrap());
                for i in 0..size {
                    let master = if size == 1 {
                        "valid-new".to_owned()
                    } else {
                        format!("valid-new-{i}")
                    };
                    let child = format!("asset-{master}");
                    let path = one_path.clone().unwrap_or_else(|| {
                        media
                            .join(run_cycle_expected_date_dir())
                            .join(format!("unique-{i}.JPG"))
                    });
                    assert_eq!(
                        std::fs::read(&path).unwrap(),
                        bytes,
                        "selected media path and bytes"
                    );
                    let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE library='PrimarySync' AND child=?1 AND master=?2 AND version_size='original'", rusqlite::params![child, master], |row| row.get(0)).unwrap();
                    assert_eq!(count, 1, "each selected rendition has exactly one receipt");
                    let row: String = conn.query_row(
                        "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,json(native_path),source_checksum,local_checksum) FROM download_receipts WHERE library='PrimarySync' AND child=?1 AND master=?2 AND version_size='original'",
                        rusqlite::params![child, master], |row| row.get(0)).unwrap();
                    let actual: serde_json::Value = serde_json::from_str(&row).unwrap();
                    let expected = serde_json::json!([
                        account_key,
                        provider_key,
                        scope,
                        "PrimarySync",
                        child,
                        master,
                        child,
                        expected_pass,
                        "original",
                        checksum,
                        bytes.len(),
                        crate::state::db::provider_selection::SelectionPath::from_path(&path),
                        content_hash,
                        content_hash
                    ]);
                    assert_eq!(
                        actual, expected,
                        "receipt must bind the selected account, scope, pass, resource and path"
                    );
                    receipts.push(actual);
                }
                if let Some(prior) = &selected_receipts {
                    assert_eq!(
                        &receipts, prior,
                        "selected receipts remain exact across held cycles"
                    );
                } else {
                    selected_receipts = Some(receipts);
                }
                eprintln!(
                    "SELECTED_RECEIPTS_REACHED recent={recent:?} size={size} cycle={cycle} exact_receipts={size}"
                );
            }
            let summary = inner.get_summary().await.unwrap();
            if let Some(before_legacy) = &legacy_before {
                assert_eq!(
                    &legacy_snapshot(&database),
                    before_legacy,
                    "prepared legacy evidence and retry survive every reopen and sparse recovery"
                );
                let capture = summary
                    .metadata_capture
                    .iter()
                    .find(|item| item.library == "PrimarySync")
                    .unwrap();
                assert_eq!(
                    (
                        capture.unattributed_legacy_assets,
                        capture.unattributed_legacy_pending
                    ),
                    (1, 1)
                );
                let status = crate::commands::backup_status_line(&summary);
                assert!(
                    status.contains(
                        "preserved legacy records have unresolved historical attribution"
                    ),
                    "{status}"
                );
                assert_eq!(
                    status.contains("unresolved asset identity"),
                    marker_expected,
                    "{status}"
                );
                eprintln!(
                    "MIXED_DEBT_REACHED recent={recent:?} cycle={cycle} sparse_pending={marker_expected} legacy_pending=1 downloaded={} primary_checkpoint_held={}",
                    result.stats.downloaded,
                    phase < recovery_phase
                );
            }
            assert_eq!(
                summary.unresolved_identity_zones,
                u64::from(marker_expected)
            );
            assert_eq!(
                crate::commands::backup_status_line(&summary).contains("unresolved asset identity"),
                marker_expected,
                "recent={recent:?} evidence={evidence:?} cycle={cycle}: {}",
                crate::commands::backup_status_line(&summary)
            );
            assert_eq!(
                inner
                    .get_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some(if phase < recovery_phase {
                    "zone-before"
                } else {
                    "zone-after"
                })
            );
            assert_eq!(
                inner
                    .get_metadata(&crate::sync_cycle::sync_token_key("SharedSync-test"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("shared-after")
            );
            if recent.is_some() && phase < recovery_phase {
                let raw = inner
                    .get_metadata("recent_selection_recovery:PrimarySync")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&raw).unwrap()["complete"],
                    false,
                    "healthy materialization never clears unrelated source identity debt"
                );
            }
            {
                let conn = inner.acquire_lock("verify recovered receipt").unwrap();
                let receipts = json_rows(
                    &conn,
                    "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,json(native_path),source_checksum,local_checksum) FROM download_receipts WHERE library='PrimarySync' AND (child='unresolved-child' OR master='recovered') ORDER BY child,master",
                );
                let valid = json_rows(
                    &conn,
                    "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,json(native_path),source_checksum,local_checksum) FROM download_receipts WHERE library='PrimarySync' AND child LIKE 'asset-valid-new%' ORDER BY child,master",
                );
                let destinations = json_rows(
                    &conn,
                    "SELECT json_array(d.generation,g.account_key,g.provider_key,g.scope,d.pass_key,d.child,d.master,d.version_size,json(d.path),d.checksum,d.size_bytes,d.admitted,d.verified_media,d.verified_metadata,d.source_checksum,d.local_checksum,d.metadata_hash) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND (d.child='unresolved-child' OR d.master='recovered') ORDER BY d.generation,d.pass_key,d.version_size",
                );
                drop(conn);
                let mut valid_sorted = valid.clone();
                valid_sorted.sort_by_key(|value| value.to_string());
                if let Some(prior) = &selected_receipts {
                    let mut prior_sorted = prior.clone();
                    prior_sorted.sort_by_key(|value| value.to_string());
                    assert_eq!(
                        valid_sorted, prior_sorted,
                        "previously published valid receipt changed"
                    );
                } else {
                    selected_receipts = Some(valid.clone());
                }
                if phase == recovery_phase && recent.is_none() && !deletion_delta {
                    assert_eq!(
                        destinations.len(),
                        1,
                        "one recovered destination recent={recent:?} evidence={evidence:?} size={size} cycle={cycle} phase={phase} stats={:?}",
                        result.stats
                    );
                    assert_eq!(
                        receipts.len(),
                        1,
                        "one recovered receipt recent={recent:?} evidence={evidence:?} size={size} cycle={cycle} phase={phase} stats={:?}",
                        result.stats
                    );
                    let destination = destinations[0].as_array().unwrap();
                    let receipt = receipts[0].as_array().unwrap();
                    let (expected_account, expected_provider) = owner.authenticated_keys().unwrap();
                    let expected_scope = crate::icloud::photos::inbox::ShadowCapture::new(
                        inner.clone(),
                        owner.clone(),
                        "com",
                    )
                    .scope(
                        "private",
                        &serde_json::json!({
                            "zoneName": "PrimarySync",
                            "ownerRecordName": "_defaultOwner"
                        }),
                    )
                    .unwrap();
                    let expected_pass = format!(
                        "{:x}",
                        Sha256::digest(
                            serde_json::json!({
                                "kind": format!("{:?}", primary.plan.passes[0].kind),
                                "scope": primary.plan.passes[0].album.selection_scope(),
                                "excludes": Vec::<String>::new(),
                            })
                            .to_string()
                            .as_bytes()
                        )
                    );
                    let content_hash = format!("{:x}", Sha256::digest(&bytes));
                    assert!(!destination[0].as_str().unwrap_or_default().is_empty());
                    let expected_destination = serde_json::json!([
                        destination[0],
                        expected_account,
                        expected_provider,
                        expected_scope.clone(),
                        expected_pass.clone(),
                        "unresolved-child",
                        "recovered",
                        "original",
                        crate::state::db::provider_selection::SelectionPath::from_path(
                            &recovered_path
                        ),
                        checksum.clone(),
                        bytes.len(),
                        1,
                        1,
                        1,
                        content_hash.clone(),
                        content_hash.clone(),
                    ]);
                    assert_eq!(&destination[..16], expected_destination.as_array().unwrap());
                    assert_eq!(&receipt[0..3], &destination[1..4]);
                    assert_eq!(receipt[4], destination[5]);
                    assert_eq!(receipt[5], destination[6]);
                    assert_eq!(receipt[6], destination[5]);
                    assert_eq!(receipt[7], destination[4]);
                    assert_eq!(receipt[8], destination[7]);
                    assert_eq!(receipt[9], destination[9]);
                    assert_eq!(receipt[10], destination[10]);
                    assert_eq!(receipt[11], destination[8]);
                    assert_eq!(receipt[12], destination[14]);
                    assert_eq!(receipt[13], destination[15]);
                    recovered_destinations_snapshot = Some(destinations.clone());
                    recovered_receipts_snapshot = Some(receipts.clone());
                } else if phase > recovery_phase && recent.is_none() && !deletion_delta {
                    assert_eq!(
                        Some(&destinations),
                        recovered_destinations_snapshot.as_ref(),
                        "recovered destination changed after recovery"
                    );
                    assert_eq!(
                        Some(&receipts),
                        recovered_receipts_snapshot.as_ref(),
                        "recovered receipt changed after recovery"
                    );
                } else {
                    assert!(
                        receipts.is_empty() && destinations.is_empty(),
                        "held, deleted or recent-only identity has recovered state: {receipts:?} {destinations:?}"
                    );
                }
            }
            let media_gets = server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/valid.jpg")
                .count();
            if cycle == 0 {
                expected_media_gets += size;
            }
            if recovery_publication {
                expected_media_gets += 1;
            }
            assert_eq!(
                media_gets, expected_media_gets,
                "recent={recent:?} evidence={evidence:?} cycle={cycle} phase={phase}"
            );
            assert_eq!(
                result.stats.downloaded,
                if cycle == 0 {
                    size
                } else if recovery_publication {
                    1
                } else {
                    0
                },
                "recent={recent:?} evidence={evidence:?} cycle={cycle}: {:?}",
                result.stats
            );
            let files = snapshot_files(&media);
            for (path, contents) in &before {
                assert_eq!(files.get(path), Some(contents));
            }
            if cycle == 0 {
                assert!(files.values().any(|contents| contents == &bytes));
                after_download = Some(files.clone());
            }
            if recovery_publication {
                assert_eq!(
                    files.len(),
                    after_download.as_ref().unwrap().len() + 1,
                    "successful recovery must add exactly one branch-scoped file"
                );
                assert_eq!(std::fs::read(&recovered_path).unwrap(), bytes);
                let prior = after_download.as_ref().unwrap();
                assert!(
                    prior
                        .iter()
                        .all(|(path, contents)| files.get(path) == Some(contents))
                );
                after_download = Some(files.clone());
            } else {
                assert_eq!(Some(files), after_download);
            }
        }
        server.verify().await;
    }
}
/// #707: the paired single-pass streaming path must gate the zone
/// checkpoint on provider-metadata durability exactly as the collecting
/// path does. Identical to the collecting regression above but without
/// `recent`, which is what routes the cycle through the streaming
/// producer rather than collecting incremental planning.
#[tokio::test]
async fn run_cycle_single_pass_metadata_refresh_failure_preserves_zone_checkpoint() {
    let config = make_run_cycle_config();
    let inner = make_state_db();
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(
            Arc::clone(&inner),
            "simulated provider metadata write failure",
        )
        .with_refresh_downloaded_metadata_failure(),
    );
    let mut page = full_album_page_with_download(
        "PrimarySync",
        "master-PrimarySync",
        "zone-tok-new",
        "https://p01.icloud-content.com/photo.jpg",
        1024,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    );
    // A metadata-only edit: the provider now reports the asset as a
    // favourite while the stored row does not.
    page["records"][1]["fields"]["isFavorite"] = serde_json::json!({"value": 1, "type": "INT64"});
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "zone-tok-new",
                "moreComing": false,
                "records": page["records"].clone()
            }]
        })),
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
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

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

    assert_eq!(result.stats.downloaded, 0);
    assert!(
        result.stats.state_write_failures > 0,
        "the streaming producer must report the failed refresh as non-durable state"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "a failed provider metadata refresh must preserve the replay checkpoint"
    );
}

/// #707: the single-pass streaming path must advance the zone checkpoint
/// once the refreshed provider metadata is durable, applying the edit
/// without downloading media.
#[tokio::test]
async fn run_cycle_single_pass_metadata_refresh_advances_checkpoint_after_durable_state() {
    let config = make_run_cycle_config();
    let inner = make_state_db();
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

    let db: Arc<dyn download::DownloadStore> =
        Arc::clone(&inner) as Arc<dyn download::DownloadStore>;
    let page = run_cycle_favourited_asset_page();
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "zone-tok-new",
                "moreComing": false,
                "records": page["records"].clone()
            }]
        })),
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
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

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

    assert_eq!(
        result.stats.downloaded, 0,
        "a metadata-only edit downloads nothing"
    );
    assert_eq!(result.stats.state_write_failures, 0);
    assert!(
        inner.get_downloaded_page(0, 1).await.unwrap()[0]
            .metadata
            .is_favorite,
        "the edited provider metadata must reach the catalogue"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-new"),
        "durable provider state must allow the zone checkpoint to advance"
    );
}

#[derive(Clone, Debug)]
struct HiddenHistoricalQuietSession {
    calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for HiddenHistoricalQuietSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/internal/records/query/batch") {
            self.calls.lock().unwrap().push("count");
            return Ok(album_count_response(0));
        }
        if url.contains("/records/query?") {
            self.calls.lock().unwrap().push("query");
            return Ok(serde_json::json!({"records": [], "syncToken": "zone-tok-new"}));
        }
        if !url.contains("/changes/zone?") {
            self.calls.lock().unwrap().push("unexpected hydration");
            anyhow::bail!("historical Hidden state must not cause provider hydration");
        }
        self.calls.lock().unwrap().push("delta");
        Ok(serde_json::json!({"zones": [{
            "zoneID": {"zoneName": "SharedSync-HIDDEN", "ownerRecordName": "_defaultOwner"},
            "syncToken": "zone-tok-new", "moreComing": false, "records": []
        }]}))
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

// Snapshot the exact scoped catalogue, mappings, owner, capture receipt and
// retry evidence, independently of the candidate-selection algorithm.
fn hidden_historical_snapshot(path: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    [
        "SELECT json_group_array(json_array(library,id,version_size,status,local_path,checksum,local_checksum,download_checksum,created_at,added_at,metadata_hash,is_hidden,is_favorite)) FROM (SELECT * FROM assets ORDER BY library,id,version_size)",
        "SELECT json_group_array(json_array(library,asset_record_name,master_record_name,updated_at)) FROM (SELECT * FROM asset_master_mappings ORDER BY library,asset_record_name,master_record_name)",
        "SELECT json_group_array(json_array(library,master_record_name,asset_record_name,claimed_at)) FROM (SELECT * FROM legacy_master_state_owners ORDER BY library,master_record_name,asset_record_name)",
        "SELECT json_group_array(json_array(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size)) FROM (SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path)",
        "SELECT json_group_array(json_array(library,asset_id,revision,updated_at)) FROM (SELECT * FROM asset_metadata_capture_revisions ORDER BY library,asset_id)",
        "SELECT json_group_array(json_array(library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at)) FROM (SELECT * FROM metadata_capture_retries ORDER BY library,asset_id)",
    ].into_iter().map(|sql| conn.query_row(sql, [], |row| row.get(0)).unwrap()).collect()
}

async fn assert_hidden_historical_state_survives_reopen(ambiguous: bool) {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig};
    use serde_json::json;

    const ZONE: &str = "SharedSync-HIDDEN";
    const MASTER: &str = "master-hidden";
    const CHILD: &str = "asset-master-hidden";
    const TOKEN: &str = "sync_token:SharedSync-HIDDEN";
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let media_dir = dir.path().join("media");
    std::fs::create_dir_all(&media_dir).unwrap();
    let hidden_path = media_dir.join("hidden.jpg");
    let primary_path = media_dir.join("primary.jpg");
    let hidden_bytes = vec![7u8; 1024];
    let primary_bytes = vec![9u8; 1024];
    std::fs::write(&hidden_path, &hidden_bytes).unwrap();
    std::fs::write(&primary_path, &primary_bytes).unwrap();
    {
        let db = state::SqliteStateDb::open(&db_path).await.unwrap();
        for (library, path) in [(ZONE, &hidden_path), ("PrimarySync", &primary_path)] {
            let record = crate::test_helpers::TestAssetRecord::new(MASTER)
                .library(library)
                .filename(if library == ZONE {
                    "hidden.jpg"
                } else {
                    "primary.jpg"
                })
                .added_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
                .size(1024)
                .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(library, MASTER, "original", path, "local-checksum", None)
                .await
                .unwrap();
            db.upsert_asset_master_mapping(library, CHILD, MASTER)
                .await
                .unwrap();
            db.set_metadata_capture_revision_for_test(library, MASTER, 0);
            if library == "PrimarySync" || !ambiguous {
                assert!(
                    db.claim_legacy_master_state_owner(library, MASTER, CHILD)
                        .await
                        .unwrap()
                );
            }
        }
        if ambiguous {
            db.upsert_asset_master_mapping(ZONE, "historical-sibling", MASTER)
                .await
                .unwrap();
            db.begin_metadata_capture_revision(ZONE, state::METADATA_CAPTURE_REVISION)
                .await
                .unwrap();
            let candidate = db
                .get_metadata_capture_candidates(ZONE, state::METADATA_CAPTURE_REVISION, 10)
                .await
                .unwrap()
                .remove(0);
            assert!(
                db.defer_metadata_capture_ambiguity(&candidate, state::METADATA_CAPTURE_REVISION)
                    .await
                    .unwrap()
            );
        }
        db.set_metadata(TOKEN, "zone-tok-prev").await.unwrap();
        db.set_metadata("sync_token:PrimarySync", "primary-before")
            .await
            .unwrap();
    }
    let before = hidden_historical_snapshot(&db_path);
    if ambiguous {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let retry: (String, String, i64, i64, i64, i64) = conn.query_row(
            "SELECT library,asset_id,target_revision,attempts,last_attempt_at,next_retry_at FROM metadata_capture_retries", [],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))
        ).unwrap();
        assert_eq!(
            (&*retry.0, &*retry.1, retry.2, retry.3),
            (ZONE, MASTER, state::METADATA_CAPTURE_REVISION, 1)
        );
        assert!(retry.5 > retry.4);
    }
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let config = make_run_cycle_config();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    for _ in 0..2 {
        let inner = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
        let db = Arc::clone(&inner) as Arc<dyn download::DownloadStore>;
        let album = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("Hidden"),
                list_type: Arc::from("CPLAssetAndMasterHiddenByAssetDate"),
                obj_type: Arc::from("CPLAssetHiddenByAssetDate"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": ZONE})),
                retry_config: retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(HiddenHistoricalQuietSession {
                calls: Arc::clone(&calls),
            }),
        );
        let lib = make_run_cycle_library_state_with_passes(
            ZONE,
            TOKEN,
            vec![crate::commands::AlbumPass {
                kind: crate::commands::PassKind::SmartFolder,
                album,
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }],
        );
        let build = make_run_cycle_download_config_builder_with_options(
            &media_dir,
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.metadata_capture_refreshed, 0);
        assert_eq!(result.stats.metadata_capture_failures, 0);
        assert_eq!(
            inner.get_metadata(TOKEN).await.unwrap().as_deref(),
            Some("zone-tok-new")
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("primary-before")
        );
        assert!(
            inner
                .get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        drop(build);
        drop(db);
        drop(inner);
        assert_eq!(hidden_historical_snapshot(&db_path), before);
        assert_eq!(std::fs::read(&hidden_path).unwrap(), hidden_bytes);
        assert_eq!(std::fs::read(&primary_path).unwrap(), primary_bytes);
        assert_eq!(std::fs::read_dir(&media_dir).unwrap().count(), 2);
    }
    let calls = calls.lock().unwrap();
    assert!(!calls.contains(&"unexpected hydration"));
    assert!(calls.iter().filter(|call| **call == "query").count() >= 2);
}

#[tokio::test]
async fn run_cycle_hidden_legacy_capture_repair_survives_restart() {
    assert_hidden_historical_state_survives_reopen(false).await;
}

#[tokio::test]
async fn run_cycle_legacy_owner_guard_preserves_dates_and_checkpoint() {
    assert_hidden_historical_state_survives_reopen(true).await;
}

#[tokio::test]
async fn run_cycle_stale_capture_revision_does_not_authorize_historical_rewrite() {
    #[derive(Clone, Debug)]
    struct CaptureRevisionSession {
        records: Arc<Vec<serde_json::Value>>,
        lookups: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for CaptureRevisionSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/lookup?") {
                self.lookups
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(serde_json::json!({"records": self.records.as_ref()}));
            }
            if url.contains("/changes/zone?") {
                return Ok(serde_json::json!({
                    "zones": [{
                        "zoneID": {
                            "zoneName": "PrimarySync",
                            "ownerRecordName": "_defaultOwner"
                        },
                        "syncToken": "zone-tok-new",
                        "moreComing": false,
                        "records": []
                    }]
                }));
            }
            Ok(serde_json::json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    let config = make_run_cycle_config();
    let inner = Arc::new(state::SqliteStateDb::open_in_memory().expect("state db"));
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db = Arc::clone(&inner) as Arc<dyn download::DownloadStore>;
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&db, download_dir.path()).await;
    inner
        .upsert_asset_master_mapping(
            "PrimarySync",
            "asset-master-PrimarySync",
            "master-PrimarySync",
        )
        .await
        .expect("seed durable provider identity");
    assert!(
        inner
            .claim_legacy_master_state_owner(
                "PrimarySync",
                "master-PrimarySync",
                "asset-master-PrimarySync",
            )
            .await
            .expect("seed legacy state owner")
    );
    inner.set_metadata_capture_revision_for_test("PrimarySync", "master-PrimarySync", 0);

    let before = inner.get_downloaded_page(0, 10).await.unwrap();
    let media_path = before[0].local_path.clone().unwrap();
    let media_bytes = std::fs::read(&media_path).unwrap();
    let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let page = run_cycle_favourited_asset_page();
    let records = page["records"]
        .as_array()
        .expect("provider page records")
        .clone();
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(CaptureRevisionSession {
            records: Arc::new(records),
            lookups: Arc::clone(&lookups),
        }),
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
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

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
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.metadata_capture_refreshed, 0);
    assert_eq!(result.stats.metadata_capture_failures, 0);
    assert_eq!(lookups.load(std::sync::atomic::Ordering::SeqCst), 0);
    let after = inner.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(
        after[0].metadata.metadata_hash,
        before[0].metadata.metadata_hash
    );
    assert!(!after[0].metadata.is_favorite);
    assert_eq!(after[0].local_path, before[0].local_path);
    assert_eq!(after[0].added_at, before[0].added_at);
    assert_eq!(std::fs::read(&media_path).unwrap(), media_bytes);
    let revision: i64 = inner.acquire_lock("inspect retained revision").unwrap().query_row(
        "SELECT revision FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2",
        rusqlite::params!["PrimarySync", "master-PrimarySync"], |row| row.get(0)).unwrap();
    assert_eq!(revision, 0);
    assert_eq!(
        inner.get_legacy_master_state_owners().await.unwrap().len(),
        1
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-new"),
        "empty replay may advance without reinterpreting historical capture"
    );
}

#[tokio::test]
async fn run_cycle_historical_capture_retry_survives_reopen_without_automatic_repair() {
    #[derive(Clone, Debug)]
    struct QuietSession {
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for QuietSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/lookup?") {
                self.calls.lock().unwrap().push("lookup");
                anyhow::bail!("historical retry must not cause provider hydration");
            }
            anyhow::ensure!(
                url.contains("/changes/zone?"),
                "unexpected provider request"
            );
            self.calls.lock().unwrap().push("delta");
            Ok(
                serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"zone-tok-new","moreComing":false,"records":[]}]}),
            )
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    fn retry_snapshot(path: &std::path::Path) -> (String, String, i64, String, i64, i64, i64, i64) {
        rusqlite::Connection::open(path).unwrap().query_row(
            "SELECT library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at FROM metadata_capture_retries",
            [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let download_dir = tempfile::tempdir().unwrap();
    let media_path = download_dir
        .path()
        .join(run_cycle_expected_date_dir())
        .join("photo.jpg");
    {
        let inner = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .unwrap();
        let db = inner.clone() as Arc<dyn download::DownloadStore>;
        seed_run_cycle_metadata_drift_asset(&db, download_dir.path()).await;
        inner.set_metadata_capture_revision_for_test("PrimarySync", "master-PrimarySync", 0);
        inner
            .begin_metadata_capture_revision("PrimarySync", state::METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        let candidate = inner
            .get_metadata_capture_candidates("PrimarySync", state::METADATA_CAPTURE_REVISION, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(
            inner
                .defer_metadata_capture_ambiguity(&candidate, state::METADATA_CAPTURE_REVISION)
                .await
                .unwrap()
        );
    }
    let evidence = retry_snapshot(&db_path);
    assert_eq!(
        (
            evidence.0.as_str(),
            evidence.1.as_str(),
            evidence.2,
            evidence.5
        ),
        (
            "PrimarySync",
            "master-PrimarySync",
            state::METADATA_CAPTURE_REVISION,
            1
        )
    );
    assert!(evidence.7 > evidence.6);
    let bytes = std::fs::read(&media_path).unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let config = make_run_cycle_config();
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    for _ in 0..2 {
        let inner = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
        let db = inner.clone() as Arc<dyn download::DownloadStore>;
        let album = make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(QuietSession {
                calls: calls.clone(),
            }),
        );
        let lib =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let build = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.metadata_capture_refreshed, 0);
        assert_eq!(result.stats.metadata_capture_failures, 0);
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-tok-new")
        );
        let rows = inner.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].metadata.is_favorite);
        assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
        assert_eq!(std::fs::read(&media_path).unwrap(), bytes);
        assert_eq!(retry_snapshot(&db_path), evidence);
        let status = inner.get_summary().await.unwrap().metadata_capture;
        assert_eq!(status.len(), 1);
        assert_eq!(
            status[0].pending_revision,
            Some(state::METADATA_CAPTURE_REVISION)
        );
        assert_eq!(status[0].unresolved_assets, 1);
    }
    assert_eq!(*calls.lock().unwrap(), vec!["delta", "delta"]);
}

/// Runs one full-enumeration cycle with XMP sidecars enabled over a
/// downloaded asset the provider now reports as a favourite.
#[cfg(feature = "xmp")]
async fn run_full_enumeration_metadata_cycle(
    inner: &Arc<dyn download::DownloadStore>,
    download_dir: &std::path::Path,
    per_pass_paths: bool,
    refresh: bool,
    controls: download::DownloadControls,
    witness: Arc<download::MetadataWriterWitness>,
) -> crate::sync_cycle::CycleResult {
    let mut config = make_run_cycle_config();
    config.runtime.refresh_metadata = refresh;
    config.metadata.xmp_sidecar = true;
    let db: Arc<dyn download::DownloadStore> = Arc::clone(inner);
    let page = run_cycle_favourited_asset_page();
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(crate::sync_loop::test_support::MutableCaptureSession {
            zone: "PrimarySync",
            cancel_after_lookup: None,
            records: Arc::new(std::sync::Mutex::new(
                page["records"].as_array().unwrap().clone(),
            )),
            repair_requests: Arc::default(),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let base = make_run_cycle_download_config_builder_with_options(
        download_dir,
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            xmp_sidecar: true,
            per_pass_paths,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&observed_modes);
    let build_download_config = move |mode: download::SyncMode, excluded, groupings, library| {
        let mut config = (*base(mode.clone(), excluded, groupings, library)).clone();
        config.refresh_metadata = refresh;
        recorder
            .lock()
            .unwrap()
            .push(format!("{mode:?}:refresh={}", config.refresh_metadata));
        Arc::new(config)
    };
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let result = Box::pin(run_cycle_with_witness(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        controls,
        &shared_session,
        &CancellationToken::new(),
        witness,
        &states
            .iter()
            .map(|state| crate::sync_cycle::ExpectedSource {
                zone_name: state.zone_name.clone(),
                baseline_scope: state.library.baseline_scope(),
                active_passes: !state.plan.passes.is_empty(),
                skip_authority: crate::sync_cycle::SkipAuthority::Reached,
            })
            .collect::<Vec<_>>(),
    ))
    .await
    .expect("run cycle");
    let modes = observed_modes.lock().unwrap().clone();
    assert!(!modes.is_empty(), "the builder must be exercised");
    assert!(
        modes.iter().all(|mode| mode.ends_with("refresh=true")),
        "every observed sync mode must receive refresh, modes={modes:?}"
    );
    if refresh {
        assert!(
            modes.iter().any(|mode| mode.starts_with("Full")),
            "the normal full producer must be observed, modes={modes:?}"
        );
    }
    eprintln!("REFRESH_MODES {modes:?}");
    result
}

/// A real normal `run_cycle` creates a fresh operation witness: one
/// pre-existing receiptless Ordinary sidecar debt is physically attempted once
/// across full inventory, bridge, and bridge fallback, with the marker intact.
#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn run_cycle_eligible_ordinary_sidecar_reports_one_physical_attempt() {
    let inner = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;
    let media_path = download_dir
        .path()
        .join(run_cycle_expected_date_dir())
        .join("photo.jpg");
    let local_sha = crate::download::file::compute_sha256(&media_path)
        .await
        .expect("hash seeded media");
    inner
        .mark_downloaded(
            "PrimarySync",
            "master-PrimarySync",
            "original",
            &media_path,
            &local_sha,
            None,
        )
        .await
        .expect("record measured local checksum");
    inner
        .record_metadata_write_failure("PrimarySync", "master-PrimarySync", "original")
        .await
        .expect("queue pre-existing Ordinary debt");
    let sidecar_path = media_path.with_file_name("photo.jpg.xmp");
    std::fs::create_dir(&sidecar_path).expect("block sidecar path");
    let attempts = download::metadata_rewrite::test_sidecar_attempts(&media_path);

    let db: Arc<dyn download::DownloadStore> = Arc::clone(&inner);
    let album = make_named_empty_full_album("PrimarySync", "TestAlbum", "zone-tok-new");
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let base = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            xmp_sidecar: true,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let build = move |mode, excluded, groupings, library| {
        let mut config = (*base(mode, excluded, groupings, library)).clone();
        config.refresh_metadata = true;
        Arc::new(config)
    };
    let mut config = make_run_cycle_config();
    config.runtime.refresh_metadata = true;
    config.metadata.xmp_sidecar = true;
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run causality cycle");

    assert_eq!(
        result.stats.exif_failures, 1,
        "the physical failure is reported once"
    );
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let pending_rows = inner
        .get_pending_metadata_rewrites_page_for_queue(
            crate::state::db::MetadataRewriteQueue::Ordinary,
            None,
            0,
            10,
        )
        .await
        .expect("read markers");
    assert_eq!(pending_rows.len(), 1);
    let pending_row = &pending_rows[0];
    assert_eq!(pending_row.asset.library.as_ref(), "PrimarySync");
    assert_eq!(pending_row.asset.id.as_ref(), "master-PrimarySync");
    assert_eq!(
        pending_row.asset.version_size,
        state::VersionSizeKey::Original
    );
    assert_eq!(pending_row.source_checksum, None);
    assert_eq!(pending_row.asset.download_checksum, None);
    assert!(pending_row.asset.local_checksum.is_some());
    assert!(pending_row.selection_receipt.is_none());
    assert!(pending_row.capture_repair_receipt.is_none());

    std::fs::remove_dir(media_path.with_file_name("photo.jpg.xmp")).expect("remove blocker");
    let repaired = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run repair cycle");
    assert_eq!(repaired.stats.exif_failures, 0);
    assert!(media_path.with_file_name("photo.jpg.xmp").is_file());
    assert!(
        inner
            .get_pending_metadata_rewrites_page_for_queue(
                crate::state::db::MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .expect("read markers")
            .is_empty()
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the repair cycle retries the marker and writes the sidecar"
    );

    let quiet = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run quiet cycle");
    assert_eq!(quiet.stats.exif_failures, 0);
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the quiet cycle performs no additional writer attempt"
    );
    eprintln!(
        "ONE_ATTEMPT pre=1 post=1 exif_failures=1 marker=pending repaired=sidecar quiet=no_writer"
    );
}

/// The actual runner carrier extends the same witness through the outer
/// refresh maintenance. Suppression is a zero new-failure tail with
/// outstanding witnessed debt, so source revalidation stays closed.
#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn runner_outer_witness_keeps_source_revalidation_closed_for_blocked_debt() {
    let inner = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;
    let media_path = download_dir
        .path()
        .join(run_cycle_expected_date_dir())
        .join("photo.jpg");
    let local_sha = crate::download::file::compute_sha256(&media_path)
        .await
        .expect("hash seeded media");
    inner
        .mark_downloaded(
            "PrimarySync",
            "master-PrimarySync",
            "original",
            &media_path,
            &local_sha,
            None,
        )
        .await
        .expect("record measured local checksum");
    inner
        .record_metadata_write_failure("PrimarySync", "master-PrimarySync", "original")
        .await
        .expect("queue pre-existing Ordinary debt");
    std::fs::create_dir(media_path.with_file_name("photo.jpg.xmp")).expect("block sidecar path");
    let attempts = download::metadata_rewrite::test_sidecar_attempts(&media_path);

    let db: Arc<dyn download::DownloadStore> = Arc::clone(&inner);
    let album = make_named_empty_full_album("PrimarySync", "TestAlbum", "zone-tok-new");
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let base = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            xmp_sidecar: true,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let build = move |mode, excluded, groupings, library| {
        let mut config = (*base(mode, excluded, groupings, library)).clone();
        config.refresh_metadata = true;
        Arc::new(config)
    };
    let mut config = make_run_cycle_config();
    config.runtime.refresh_metadata = true;
    config.metadata.xmp_sidecar = true;
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let witness = Arc::new(download::MetadataWriterWitness::default());

    let result = run_cycle_with_witness(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
        Arc::clone(&witness),
        &[crate::sync_cycle::ExpectedSource {
            zone_name: lib_state.zone_name.clone(),
            baseline_scope: lib_state.library.baseline_scope(),
            active_passes: !lib_state.plan.passes.is_empty(),
            skip_authority: crate::sync_cycle::SkipAuthority::Reached,
        }],
    )
    .await
    .expect("run witnessed cycle");
    let tail = download::drain_pending_metadata_rewrites_witnessed(
        db.as_ref(),
        download_dir.path(),
        &config.metadata,
        download::CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        Arc::from(".kei-tmp"),
        &CancellationToken::new(),
        Some(witness),
    )
    .await;

    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the composed operation makes one physical attempt"
    );
    assert_eq!(result.stats.exif_failures, 1);
    assert_eq!(tail.new_failures, 0);
    assert!(
        tail.witnessed_blocked > 0,
        "the outer drain suppressed the already-witnessed blocked attempt"
    );
    assert!(
        !crate::sync_loop::runner::refresh_tail_allows_source_revalidation(&tail),
        "source revalidation must remain closed while witnessed debt is outstanding"
    );
    assert!(
        crate::sync_loop::runner::refresh_tail_allows_source_revalidation(
            &download::RefreshTailOutcome::default()
        ),
        "a clean tail still allows source revalidation"
    );
    eprintln!(
        "OUTER_GATE pre=1 new_failures=0 witnessed_blocked={} source_revalidation=closed clean_positive=open",
        tail.witnessed_blocked
    );
}

/// #707: each producer drains once. Unix witnesses suppress a duplicate across
/// full + bridge producers; unavailable witnesses preserve normal guarded retry.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_full_enumeration_reports_a_failed_rewrite_once() {
    for (per_pass_paths, unavailable) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let inner = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;
        // A directory where the sidecar belongs makes the rewrite fail.
        std::fs::create_dir(
            download_dir
                .path()
                .join(run_cycle_expected_date_dir())
                .join("photo.jpg.xmp"),
        )
        .expect("block the sidecar path");
        let path = download_dir
            .path()
            .join(run_cycle_expected_date_dir())
            .join("photo.jpg");
        let physical = download::metadata_rewrite::test_sidecar_attempts(&path);
        let counting = FailingMetadataSetDb::without_set_failure(Arc::clone(&inner), "unused");
        let drains = counting.drain_counter();
        let db: Arc<dyn download::DownloadStore> = Arc::new(counting);
        #[cfg(unix)]
        let witness = if unavailable {
            download::MetadataWriterWitness::with_test_budget(0, 0)
        } else {
            download::MetadataWriterWitness::default()
        };
        #[cfg(not(unix))]
        let witness = download::MetadataWriterWitness::default();
        let expected = if cfg!(unix) && !unavailable { 1 } else { 2 };

        let result = run_full_enumeration_metadata_cycle(
            &db,
            download_dir.path(),
            per_pass_paths,
            true,
            download::DownloadControls::download_hidden(),
            Arc::new(witness),
        )
        .await;

        assert_eq!(
            result.stats.exif_failures, expected,
            "composed attempts (per_pass_paths={per_pass_paths}, unavailable={unavailable})"
        );
        assert_eq!(physical.load(std::sync::atomic::Ordering::SeqCst), expected);
        assert_eq!(
            drains.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one drain per producer"
        );
        eprintln!(
            "PRODUCER_RETRY per_pass={per_pass_paths} unavailable={unavailable} physical={expected} drains=2"
        );
        assert!(
            !inner
                .get_pending_metadata_rewrites_page_for_queue(
                    crate::state::db::MetadataRewriteQueue::Ordinary,
                    None,
                    0,
                    10
                )
                .await
                .expect("read markers")
                .is_empty(),
            "the marker must survive for the next run to retry"
        );
    }
}

/// #707 review: the interleaving the maintainer described. Two passes run
/// concurrently over one asset carrying different provider snapshots. They
/// must share one drain, so a single writer owns the file, and the sidecar
/// must agree with whichever snapshot the row settled on with no marker
/// left claiming otherwise.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_two_passes_over_one_asset_leave_the_sidecar_matching_the_row() {
    let mut config = make_run_cycle_config();
    config.runtime.refresh_metadata = true;
    config.metadata.xmp_sidecar = true;
    let inner = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

    let counting = FailingMetadataSetDb::without_set_failure(
        Arc::clone(&inner),
        "__unused_metadata_message__",
    );
    let drains = counting.drain_counter();
    let db: Arc<dyn download::DownloadStore> = Arc::new(counting);
    let pass = |name: &str, page: serde_json::Value| crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Album,
        album: make_named_full_album_with_boxed_session(
            "PrimarySync",
            name,
            Box::new(
                crate::test_helpers::MockPhotosSession::new()
                    .ok(album_count_response(1))
                    .ok(page),
            ),
        ),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    };
    let lib_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![
            pass("Favourited", run_cycle_favourited_asset_page()),
            pass("Captioned", run_cycle_captioned_asset_page()),
        ],
    );
    let base = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            xmp_sidecar: true,
            per_pass_paths: true,
            concurrent_downloads: Some(2),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let build_download_config = move |mode: download::SyncMode, excluded, groupings, library| {
        let mut config = (*base(mode, excluded, groupings, library)).clone();
        config.refresh_metadata = true;
        Arc::new(config)
    };
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    run_cycle(
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
    .expect("run cycle");

    let stored = inner
        .get_downloaded_page(0, 10)
        .await
        .expect("read downloaded rows");
    let row = stored.first().expect("the asset stays downloaded");
    let sidecar = std::fs::read_to_string(
        download_dir
            .path()
            .join(run_cycle_expected_date_dir())
            .join("photo.jpg.xmp"),
    )
    .expect("the drain writes a sidecar");

    assert_eq!(
        drains.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "each producer drains once: the two passes share the full-enumeration drain and the bridge is the second producer"
    );
    assert_eq!(
        sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
        row.metadata.rating == Some(5),
        "the sidecar must hold the snapshot the row settled on: {sidecar}"
    );
    assert!(
        inner
            .get_metadata_retry_markers()
            .await
            .expect("read markers")
            .is_empty(),
        "no marker may survive a completed drain"
    );
}

/// A direct metadata capture between publication and finalization must leave
/// its newer snapshot and rewrite marker intact. This models the state
/// interleaving, not a second provider scan or a completed refresh.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn older_download_finalization_preserves_newer_metadata_marker() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let server = crate::start_wiremock_or_skip!();
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("state.db");
    let owner = state::db::account::AccountOwner::authenticated(
        "race@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-race"}})).unwrap(),
    )
    .unwrap();
    let sqlite = Arc::new(
        state::SqliteStateDb::open_owned(&state_path, &owner)
            .await
            .unwrap(),
    );
    let inner: Arc<dyn download::DownloadStore> = sqlite.clone();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let body = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    let media_dir = download_dir.path().join(run_cycle_expected_date_dir());
    std::fs::create_dir_all(&media_dir).unwrap();
    let seeded_path = media_dir.join("cGhvdG8uanBn.JPG");
    std::fs::write(&seeded_path, body).unwrap();
    let mut metadata = state::AssetMetadata::default();
    metadata.refresh_hash();
    let record = crate::test_helpers::TestAssetRecord::new("master-PrimarySync")
        .filename("photo.jpg")
        .added_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
        .created_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
        .checksum(&checksum)
        .size(body.len() as u64)
        .metadata(metadata)
        .build();
    inner.upsert_seen(&record).await.unwrap();
    inner
        .mark_downloaded(
            "PrimarySync",
            "master-PrimarySync",
            "original",
            &seeded_path,
            &data_encoding::HEXLOWER.encode(&Sha256::digest(body)),
            None,
        )
        .await
        .unwrap();

    // The row stays downloaded but its file is gone, so the media task is
    // forwarded rather than skipped as already on disk.
    std::fs::remove_file(&seeded_path).expect("remove the media file");

    let db: Arc<dyn download::DownloadStore> = sqlite.clone();
    let probe = crate::download::finalize::finalization_probe::BeforeFinalizationProbe::for_path(
        &seeded_path,
    );

    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .mount(&server)
        .await;

    #[derive(Clone, Debug)]
    struct RaceSession {
        page: serde_json::Value,
        queries: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for RaceSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/lookup?") {
                let request: serde_json::Value = serde_json::from_str(&body)?;
                let names: Vec<_> = request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|record| record["recordName"].as_str())
                    .collect();
                let records: Vec<_> = self.page["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|record| {
                        record["recordName"]
                            .as_str()
                            .is_some_and(|name| names.contains(&name))
                    })
                    .cloned()
                    .collect();
                return Ok(serde_json::json!({"records": records}));
            }
            if url.contains("/records/query/batch?") {
                return Ok(album_count_response(1));
            }
            if url.contains("/changes/zone?") {
                return Ok(
                    serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"zone-tok-new","moreComing":false,"records":[]}]}),
                );
            }
            if url.contains("/records/query?")
                && self
                    .queries
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
            {
                return Ok(self.page.clone());
            }
            Ok(serde_json::json!({"records":[]}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    fn scoped_album(
        session: RaceSession,
        db: Arc<state::SqliteStateDb>,
        owner: state::db::account::AccountOwner,
    ) -> crate::icloud::photos::PhotoAlbum {
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(session),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db, owner, "com"),
            Arc::from("private"),
        );
        album
    }
    let album = scoped_album(
        RaceSession {
            page: full_album_page_with_download(
                "PrimarySync",
                "master-PrimarySync",
                "zone-tok-new",
                &format!("{}/photo.jpg", server.uri()),
                body.len() as u64,
                &checksum,
            ),
            queries: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
        sqlite.clone(),
        owner.clone(),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let pass = &lib_state.plan.passes[0];
    let expected_pass = format!(
        "{:x}",
        Sha256::digest(
            serde_json::json!({
                "kind": format!("{:?}", pass.kind),
                "scope": pass.album.selection_scope(),
                "excludes": Vec::<String>::new(),
            })
            .to_string()
            .as_bytes()
        )
    );
    let expected_scope =
        crate::icloud::photos::inbox::ShadowCapture::new(sqlite.clone(), owner.clone(), "com")
            .scope(
                "private",
                &serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            )
            .unwrap();
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            xmp_sidecar: true,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let cancel = CancellationToken::new();
    let older = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &cancel,
    );
    let newer_pass = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), probe.observed())
            .await
            .expect("older finalization reached");
        let before = db.get_downloaded_page(0, 10).await.unwrap();
        let old_hash = before[0].metadata.metadata_hash.clone().unwrap();
        let old_marker: i64 = {
            let conn = sqlite.acquire_lock("old rewrite marker").unwrap();
            conn.query_row("SELECT metadata_write_failed_at FROM assets WHERE library='PrimarySync' AND id='master-PrimarySync' AND version_size='original'", [], |r| r.get(0)).unwrap()
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while chrono::Utc::now().timestamp() <= old_marker {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("clock advanced beyond the old marker");
        let mut page = full_album_page_with_download(
            "PrimarySync",
            "master-PrimarySync",
            "zone-tok-newer",
            &format!("{}/photo.jpg", server.uri()),
            body.len() as u64,
            &checksum,
        );
        page["records"][1]["fields"]["captionEnc"] =
            serde_json::json!({"value":"newer provider caption", "type":"STRING"});
        page["records"][1]["recordChangeTag"] = serde_json::json!("ct-asset-newer");

        // Model a provider metadata observation at the same production transaction
        // used by the pipeline; this is not a second provider scan.
        let asset = crate::icloud::photos::PhotoAsset::new(
            page["records"][0].clone(),
            page["records"][1].clone(),
        );
        let capture = crate::download::filter::metadata_capture(&asset);
        let updated = db
            .refresh_downloaded_asset_metadata(
                "PrimarySync",
                asset.state_id(),
                (&capture, asset.created(), Some(asset.added_date())),
                true,
                false,
                state::METADATA_CAPTURE_REVISION,
            )
            .await;
        assert_eq!(updated.unwrap(), 1);
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(
            rows[0].metadata.title.as_deref(),
            Some("newer provider caption")
        );
        assert_ne!(
            rows[0].metadata.metadata_hash.as_deref(),
            Some(old_hash.as_str())
        );
        assert!(db.get_metadata_retry_markers().await.unwrap().contains(&(
            "PrimarySync".into(),
            "master-PrimarySync".into(),
            "original".into()
        )));
        let new_hash = rows[0].metadata.metadata_hash.clone().unwrap();
        let new_marker: i64 = {
            let conn = sqlite.acquire_lock("new rewrite marker").unwrap();
            conn.query_row("SELECT metadata_write_failed_at FROM assets WHERE library='PrimarySync' AND id='master-PrimarySync' AND version_size='original'", [], |r| r.get(0)).unwrap()
        };
        assert!(new_marker > old_marker);
        probe.release();
        (new_hash, new_marker)
    };
    let (result, (new_hash, new_marker)) = tokio::join!(older, newer_pass);
    let result = result.expect("run cycle");
    drop(probe);
    assert_eq!(result.stats.downloaded, 1);
    assert!(
        inner
            .get_metadata_retry_markers()
            .await
            .unwrap()
            .contains(&(
                "PrimarySync".into(),
                "master-PrimarySync".into(),
                "original".into()
            ))
    );
    let row = inner
        .get_downloaded_page(0, 10)
        .await
        .expect("read downloaded rows")
        .into_iter()
        .next()
        .expect("the asset stays downloaded");
    assert_eq!(
        row.metadata.title.as_deref(),
        Some("newer provider caption"),
        "the later capture leaves the newer snapshot on the row"
    );

    let published = row.local_path.as_deref().expect("the download published");
    let mut sidecar_name = published.file_name().expect("a file name").to_os_string();
    sidecar_name.push(".xmp");
    let sidecar = std::fs::read_to_string(published.with_file_name(&sidecar_name))
        .expect("sidecar written next to the media file");
    assert!(
        !sidecar.contains("newer provider caption"),
        "older download must contain its planned snapshot"
    );

    let published = published.to_path_buf();
    let sidecar_path = published.with_file_name(sidecar_name);
    let expected_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(body));
    assert_eq!(std::fs::read(&published).unwrap(), body);
    assert_eq!(row.local_checksum.as_deref(), Some(expected_hash.as_str()));
    assert_eq!(
        row.download_checksum.as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(
        row.metadata.metadata_hash.as_deref(),
        Some(new_hash.as_str())
    );
    // The synthetic interleaving leaves the later bridge unable to project;
    // retain that real failure and its checkpoint veto rather than treating it
    // as successful completion of a second provider pass.
    assert_eq!(result.failed_count, 1, "{result:?}");
    assert_eq!(result.stats.state_write_failures, 1);
    assert!(trace.contains_event(|event| event.message()
        == Some("Selection projection refused canonical mutation")
        && event.field("error")
            == Some("Could not update the state database schema: Query returned no rows")));
    assert!(!result.db_sync_token_advance_safe);
    let receipt: String = {
        let conn = sqlite.acquire_lock("scoped source receipt").unwrap();
        conn.query_row(
            "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child=?1 AND master=?2 AND version_size='original'",
            ["asset-master-PrimarySync", "master-PrimarySync"], |r| r.get(0),
        ).unwrap()
    };
    let fields: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    for index in [0, 1, 2, 7, 11, 14] {
        assert!(!fields[index].as_str().unwrap().is_empty());
    }
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    assert_eq!(fields[0], account_key);
    assert_eq!(fields[1], provider_key);
    assert_eq!(fields[2], expected_scope);
    assert_eq!(fields[7], expected_pass);
    assert_eq!(
        fields[11],
        crate::state::db::provider_selection::SelectionPath::from_path(&published)
            .key()
            .unwrap()
    );
    assert_eq!(fields[3], "PrimarySync");
    assert_eq!(fields[4], "asset-master-PrimarySync");
    assert_eq!(fields[5], "master-PrimarySync");
    assert_eq!(fields[8], "original");
    assert_eq!(fields[9], checksum);
    assert_eq!(fields[10], body.len());
    assert_eq!(fields[12], expected_hash);
    assert_eq!(fields[13], expected_hash);
    drop(row);
    drop(build_download_config);
    drop(db);
    drop(inner);
    drop(sqlite);

    let reopened = Arc::new(
        state::SqliteStateDb::open_owned(&state_path, &owner)
            .await
            .unwrap(),
    );
    let db: Arc<dyn download::DownloadStore> = reopened.clone();
    assert!(db.get_metadata_retry_markers().await.unwrap().contains(&(
        "PrimarySync".into(),
        "master-PrimarySync".into(),
        "original".into()
    )));
    let before = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(
        before[0].metadata.title.as_deref(),
        Some("newer provider caption")
    );
    assert_eq!(
        before[0].local_checksum.as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(
        before[0].download_checksum.as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(std::fs::read(&published).unwrap(), body);
    assert_eq!(std::fs::read_to_string(&sidecar_path).unwrap(), sidecar);

    assert_eq!(
        before[0].metadata.metadata_hash.as_deref(),
        Some(new_hash.as_str())
    );
    let retained_receipt: String = {
        let conn = reopened
            .acquire_lock("reopened scoped source receipt")
            .unwrap();
        conn.query_row(
            "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child=?1 AND master=?2 AND version_size='original'",
            ["asset-master-PrimarySync", "master-PrimarySync"], |r| r.get(0),
        ).unwrap()
    };
    assert_eq!(retained_receipt, receipt);
    let retained_marker: i64 = {
        let conn = reopened.acquire_lock("reopened rewrite marker").unwrap();
        conn.query_row("SELECT metadata_write_failed_at FROM assets WHERE library='PrimarySync' AND id='master-PrimarySync' AND version_size='original'", [], |r| r.get(0)).unwrap()
    };
    assert_eq!(retained_marker, new_marker);
    // The later serialized cycle sees provider-confirmed newer metadata and
    // an explicit refresh request. Check physical convergence and retained debt.
    let mut page = full_album_page_with_download(
        "PrimarySync",
        "master-PrimarySync",
        "zone-tok-newer",
        &format!("{}/photo.jpg", server.uri()),
        body.len() as u64,
        &checksum,
    );
    page["records"][1]["fields"]["captionEnc"] =
        serde_json::json!({"value":"newer provider caption", "type":"STRING"});
    page["records"][1]["recordChangeTag"] = serde_json::json!("ct-asset-newer");
    let album = scoped_album(
        RaceSession {
            page: page.clone(),
            queries: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
        reopened.clone(),
        owner.clone(),
    );
    let lib =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let mut refresh_config = make_run_cycle_config();
    refresh_config.runtime.refresh_metadata = true;
    refresh_config.metadata.xmp_sidecar = true;
    let build = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        db.clone(),
        RunCycleDownloadConfigOptions {
            xmp_sidecar: true,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let refresh_build = |mode, excluded, groupings, library| {
        let mut cfg = (*build(mode, excluded, groupings, library)).clone();
        cfg.refresh_metadata = true;
        Arc::new(cfg)
    };
    let recovery = run_cycle(
        &[&lib],
        &refresh_config,
        Some(db.as_ref()),
        false,
        &refresh_build,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(recovery.failed_count, 1, "{recovery:?}");
    assert_eq!(recovery.stats.skipped.uncertain, 1);
    assert!(trace.contains_event(|event| event.message()
        == Some("Previously selected work remains pending after confirmation refusal")));
    let pending_selection: i64 = {
        let conn = reopened
            .acquire_lock("retained selection after refresh")
            .unwrap();
        conn.query_row(
            "SELECT count(*) FROM provider_active_destinations d JOIN provider_active_decisions s ON s.generation=d.generation AND s.pass_key=d.pass_key AND s.child=d.child WHERE (d.verified_media=0 OR d.verified_metadata=0) AND s.reason='current_confirmation_unavailable'",
            [], |r| r.get(0),
        ).unwrap()
    };
    assert!(
        pending_selection > 0,
        "the refused selected work must remain durable"
    );
    assert_eq!(recovery.stats.downloaded, 0);
    assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
    assert_eq!(std::fs::read(&published).unwrap(), body);
    assert!(
        std::fs::read_to_string(&sidecar_path)
            .unwrap()
            .contains("newer provider caption")
    );
}

/// #707 review: deferring the drain moved it outside the pipeline, which
/// returns early for the read-only run modes. The drain writes sidecars,
/// so it must stay behind the same gate.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_full_enumeration_writes_no_metadata_in_read_only_modes() {
    for controls in [
        download::DownloadControls::dry_run_hidden(),
        download::DownloadControls::new(
            download::DownloadRunMode::PrintFilenames,
            download::DownloadReporting::hidden(),
        ),
    ] {
        let inner = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;
        inner
            .record_metadata_write_failure("PrimarySync", "master-PrimarySync", "original")
            .await
            .expect("queue a rewrite");

        run_full_enumeration_metadata_cycle(
            &inner,
            download_dir.path(),
            false,
            true,
            controls,
            Arc::new(download::MetadataWriterWitness::default()),
        )
        .await;

        assert!(
            !download_dir
                .path()
                .join(run_cycle_expected_date_dir())
                .join("photo.jpg.xmp")
                .exists(),
            "a read-only run must not write a sidecar"
        );
        assert!(
            !inner
                .get_pending_metadata_rewrites_page_for_queue(
                    crate::state::db::MetadataRewriteQueue::Ordinary,
                    None,
                    0,
                    10
                )
                .await
                .expect("read markers")
                .is_empty(),
            "a read-only run must not retire the marker"
        );
    }
}

/// #707: the full-enumeration path shares the streaming producer with the
/// single-pass path, so a failed provider-metadata refresh must equally
/// stop the zone checkpoint from being stored.
#[tokio::test]
async fn run_cycle_full_enumeration_metadata_refresh_failure_preserves_zone_checkpoint() {
    let config = make_run_cycle_config();
    let inner = make_state_db();
    // No seeded zone token, so the cycle runs a full enumeration.
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(
            Arc::clone(&inner),
            "simulated provider metadata write failure",
        )
        .with_refresh_downloaded_metadata_failure(),
    );
    let page = run_cycle_favourited_asset_page();
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(page),
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
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

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

    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::NoStoredToken),
        "this regression must exercise the full-enumeration path"
    );
    assert!(
        result.stats.state_write_failures > 0,
        "the full-enumeration producer must report the failed refresh as non-durable state"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token"),
        None,
        "a failed provider metadata refresh must not store a zone checkpoint"
    );
}

#[tokio::test]
async fn run_cycle_provider_session_failures_preserve_and_recover_checkpoints() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let snapshot_files = |directory: &std::path::Path| {
        let mut files = std::collections::BTreeMap::new();
        let mut directories = vec![directory.to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    directories.push(path);
                } else {
                    files.insert(path.clone(), std::fs::read(path).unwrap());
                }
            }
        }
        files
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Stage {
        Incremental,
        Full,
        FullPerPass,
        Capture,
        LegacyCapture,
        AssetPair,
        AssetIdentity,
    }

    #[derive(Clone, Debug)]
    struct RecoverySession {
        stage: Stage,
        status: u16,
        failing: Arc<AtomicBool>,
        queries: Arc<AtomicUsize>,
        lookups: Arc<AtomicUsize>,
        records: Vec<serde_json::Value>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for RecoverySession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let lookup = url.contains("/records/lookup?");
            let changes = url.contains("/changes/zone?");
            let query = url.contains("/records/query?");
            if lookup {
                self.lookups.fetch_add(1, Ordering::SeqCst);
            }
            if query {
                self.queries.fetch_add(1, Ordering::SeqCst);
            }
            if matches!(self.stage, Stage::Full | Stage::FullPerPass) && changes {
                return Err(crate::icloud::photos::SyncTokenError::InvalidToken {
                    reason: "test requires full enumeration".into(),
                }
                .into());
            }
            let reject = match self.stage {
                Stage::Incremental | Stage::LegacyCapture => changes,
                Stage::Full | Stage::FullPerPass => query,
                Stage::Capture | Stage::AssetPair | Stage::AssetIdentity => lookup,
            };
            if reject && self.failing.load(Ordering::SeqCst) {
                return Err(crate::icloud::photos::session::HttpStatusError {
                    status: self.status,
                    url: "https://example.invalid/private?token=secret".into(),
                    retry_after: None,
                    body: Some("Invalid global session".into()),
                }
                .into());
            }
            if lookup {
                return Ok(serde_json::json!({"records": self.records}));
            }
            if changes {
                let records = match self.stage {
                    Stage::AssetPair => vec![self.records[1].clone()],
                    Stage::AssetIdentity => {
                        let mut child = self.records[1].clone();
                        child["fields"].as_object_mut().unwrap().remove("masterRef");
                        vec![child]
                    }
                    Stage::LegacyCapture => self.records.clone(),
                    _ => Vec::new(),
                };
                return Ok(serde_json::json!({"zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-after", "moreComing": false, "records": records
                }]}));
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(1));
            }
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.clone()
            } else {
                Vec::new()
            };
            Ok(serde_json::json!({"records": records, "syncToken": "zone-after"}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    for stage in [
        Stage::Incremental,
        Stage::Full,
        Stage::FullPerPass,
        Stage::Capture,
        Stage::LegacyCapture,
        Stage::AssetPair,
        Stage::AssetIdentity,
    ] {
        for status in [401, 403, 421] {
            for recent in [None, Some(10)] {
                // A bounded full inventory deliberately cannot advance its checkpoint.
                if matches!(stage, Stage::Full | Stage::FullPerPass) && recent.is_some() {
                    continue;
                }
                let mut config = make_run_cycle_config();
                config.filters.recent = recent;
                let dir = tempfile::tempdir().unwrap();
                let inner = Arc::new(
                    state::SqliteStateDb::open(&dir.path().join("state.db"))
                        .await
                        .unwrap(),
                );
                let db = inner.clone() as Arc<dyn download::DownloadStore>;
                let media_dir = dir.path().join("media");
                seed_run_cycle_metadata_drift_asset(&db, &media_dir).await;
                if stage != Stage::AssetIdentity {
                    inner
                        .upsert_asset_master_mapping(
                            "PrimarySync",
                            "asset-master-PrimarySync",
                            "master-PrimarySync",
                        )
                        .await
                        .unwrap();
                    if stage != Stage::LegacyCapture {
                        assert!(
                            inner
                                .claim_legacy_master_state_owner(
                                    "PrimarySync",
                                    "master-PrimarySync",
                                    "asset-master-PrimarySync"
                                )
                                .await
                                .unwrap()
                        );
                    }
                }
                let capture_pending = matches!(stage, Stage::Capture | Stage::LegacyCapture);
                inner.set_metadata_capture_revision_for_test(
                    "PrimarySync",
                    "master-PrimarySync",
                    if capture_pending {
                        0
                    } else {
                        state::METADATA_CAPTURE_REVISION
                    },
                );
                inner
                    .set_metadata("sync_token:PrimarySync", "zone-before")
                    .await
                    .unwrap();
                let before = snapshot_files(&media_dir);
                let failing = Arc::new(AtomicBool::new(true));
                let queries = Arc::new(AtomicUsize::new(0));
                let lookups = Arc::new(AtomicUsize::new(0));
                let album = make_full_album_with_boxed_session(
                    "PrimarySync",
                    Box::new(RecoverySession {
                        stage,
                        status,
                        failing: failing.clone(),
                        queries: queries.clone(),
                        lookups: lookups.clone(),
                        records: run_cycle_favourited_asset_page()["records"]
                            .as_array()
                            .unwrap()
                            .clone(),
                    }),
                );
                let library = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    album,
                );
                let builder = make_run_cycle_download_config_builder_with_options(
                    &media_dir,
                    db.clone(),
                    RunCycleDownloadConfigOptions {
                        media: media_without_photo_downloads(),
                        per_pass_paths: stage == Stage::FullPerPass,
                        recent,
                        ..RunCycleDownloadConfigOptions::default()
                    },
                );
                let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
                for cycle in 0..3 {
                    if cycle > 0 {
                        failing.store(false, Ordering::SeqCst);
                    }
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
                    // Historical capture is no longer requested by ordinary sync.
                    // Its lookup failure must not masquerade as a current auth failure.
                    let current_auth_failure = cycle == 0 && stage != Stage::Capture;
                    assert_eq!(
                        result.session_expired, current_auth_failure,
                        "{stage:?} HTTP {status} recent={recent:?} cycle={cycle}"
                    );
                    assert_eq!(
                        inner
                            .get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some(if current_auth_failure {
                            "zone-before"
                        } else {
                            "zone-after"
                        })
                    );
                    assert_eq!(
                        snapshot_files(&media_dir),
                        before,
                        "provider recovery must not change local media"
                    );
                    assert_eq!(result.stats.downloaded, 0);
                    if recent.is_none()
                        && matches!(
                            stage,
                            Stage::Incremental | Stage::AssetPair | Stage::AssetIdentity
                        )
                    {
                        let summary = inner.get_summary().await.unwrap();
                        let status = crate::commands::backup_status_line(&summary);
                        assert_eq!(summary.last_sync_interrupted, cycle == 0);
                        assert_eq!(
                            summary.last_sync_status.as_deref(),
                            Some(if cycle == 0 {
                                "interrupted"
                            } else {
                                "complete"
                            })
                        );
                        assert_eq!(summary.last_sync_enumeration_errors, u64::from(cycle == 0));
                        if cycle == 0 {
                            assert_eq!(
                                status,
                                if matches!(stage, Stage::AssetPair | Stage::AssetIdentity) {
                                    "Recorded run/database notes: unresolved asset identity in 1 provider zone; historical and current associations are reported separately below; last sync was interrupted; 1 enumeration error occurred in the last sync"
                                } else {
                                    "Recorded run/database notes: last sync was interrupted; 1 enumeration error occurred in the last sync"
                                }
                            );
                        } else {
                            assert_eq!(
                                status,
                                "Recorded run/database notes: last sync recorded completion and no pending or failed catalog rows remain; this does not prove coverage"
                            );
                        }
                    }
                    if current_auth_failure {
                        assert!(!result.db_sync_token_advance_safe);
                        assert_eq!(
                            inner
                                .get_metadata("last_recovery_action")
                                .await
                                .unwrap()
                                .as_deref(),
                            Some("reauthenticate")
                        );
                        if !matches!(stage, Stage::Full | Stage::FullPerPass) {
                            assert_eq!(queries.load(Ordering::SeqCst), 0);
                        }
                        if capture_pending {
                            assert!(
                                inner
                                    .has_metadata_capture_work(
                                        &["PrimarySync"],
                                        state::METADATA_CAPTURE_REVISION
                                    )
                                    .await
                                    .unwrap()
                            );
                        }
                    } else {
                        assert_eq!(result.failed_count, 0);
                    }
                    if capture_pending {
                        assert_eq!(result.stats.metadata_capture_refreshed, 0);
                    }
                    if stage == Stage::LegacyCapture && cycle > 0 {
                        assert!(
                            inner.get_downloaded_page(0, 1).await.unwrap()[0]
                                .metadata
                                .is_favorite,
                            "current provider metadata must still refresh"
                        );
                    }
                    // A quiet delta has no new metadata evidence. Bounded inventory
                    // and LegacyCapture's live records may legitimately refresh the
                    // database revision without initiating historical file writes.
                    if stage == Stage::Capture && recent.is_none() {
                        assert!(
                            inner
                                .has_metadata_capture_work(
                                    &["PrimarySync"],
                                    state::METADATA_CAPTURE_REVISION
                                )
                                .await
                                .unwrap(),
                            "historical capture debt: {stage:?} HTTP {status} recent={recent:?} cycle={cycle}"
                        );
                    }
                }
                if stage == Stage::Capture {
                    assert_eq!(lookups.load(Ordering::SeqCst), 0);
                }
            }
        }
    }
}

#[tokio::test]
async fn sparse_deletion_batches_survive_restart_and_preserve_failed_state() {
    use crate::state::{SparseIdentityStore, SparseSourceId};
    #[derive(Clone, Debug)]
    struct DeletedSession {
        lookups: Arc<std::sync::atomic::AtomicUsize>,
        replay: bool,
        count: usize,
        snapshot: Arc<std::sync::atomic::AtomicUsize>,
        changing_snapshot: bool,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for DeletedSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            if let Some(page) = sparse_fixture_inventory(url, &body, &[]) {
                return Ok(page);
            }
            if url.contains("/changes/zone?") {
                let mut records = Vec::new();
                if self.replay && body["zones"][0]["syncToken"] == "before" {
                    for index in 0..self.count {
                        let mut record = crate::test_helpers::sparse_shared_asset_record();
                        record["recordName"] = serde_json::json!(format!("source-{index:03}"));
                        records.push(record);
                    }
                }
                let snapshot = if self.changing_snapshot {
                    format!(
                        "after-{}",
                        self.snapshot.load(std::sync::atomic::Ordering::Relaxed)
                    )
                } else {
                    "after".into()
                };
                // Unrelated activity changes the zone token, not the source facts.
                if self.changing_snapshot {
                    records.push(serde_json::json!({"recordName":format!("unrelated-{snapshot}"),"deleted":true}));
                }
                return Ok(
                    serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":snapshot,"moreComing":false,"records":records}]}),
                );
            }
            assert!(url.contains("/records/lookup?"));
            let records: Vec<_> = body["records"].as_array().unwrap().iter().map(|r| {
                self.lookups.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                serde_json::json!({"recordName": r["recordName"], "serverErrorCode":"UNKNOWN_ITEM"})
            }).collect();
            Ok(serde_json::json!({"records":records}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    for recent in [None, Some(10)] {
        for replay in [false, true] {
            for changing_snapshot in [false, true] {
                let count = 101;
                let directory = tempfile::tempdir().unwrap();
                let media = directory.path().join("media");
                let database = directory.path().join("state.db");
                {
                    let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
                    let db = inner.clone() as Arc<dyn download::DownloadStore>;
                    seed_run_cycle_metadata_drift_asset(&db, &media).await;
                    inner.set_metadata_capture_revision_for_test(
                        "PrimarySync",
                        "master-PrimarySync",
                        state::METADATA_CAPTURE_REVISION,
                    );
                    let run = db.start_sync_run().await.unwrap();
                    db.complete_sync_run(run, &state::SyncRunStats::default())
                        .await
                        .unwrap();
                    db.set_metadata("sync_token:PrimarySync", "before")
                        .await
                        .unwrap();
                    let evidence = crate::icloud::photos::asset::SparseShareEvidence::from_fields(
                        &crate::test_helpers::sparse_shared_asset_record()["fields"],
                    )
                    .unwrap()
                    .durable_key()
                    .unwrap();
                    for index in 0..count {
                        db.observe_sparse_identity(
                            "PrimarySync",
                            &SparseSourceId::new(&format!("source-{index:03}")),
                            &evidence,
                            chrono::Utc::now(),
                        )
                        .await
                        .unwrap();
                    }
                    // Force the cached deletion through the real source-state update after restart.
                    let record = crate::test_helpers::TestAssetRecord::new("source-000").build();
                    db.upsert_seen(&record).await.unwrap();
                    inner.set_metadata_capture_revision_for_test(
                        "PrimarySync",
                        "source-000",
                        state::METADATA_CAPTURE_REVISION,
                    );
                }
                let sentinel = media.join("keep.bin");
                std::fs::write(&sentinel, b"unchanged private media").unwrap();
                let mut config = make_run_cycle_config();
                config.filters.recent = recent;
                let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
                let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let snapshot = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let library = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    make_full_album_with_boxed_session(
                        "PrimarySync",
                        Box::new(DeletedSession {
                            lookups: lookups.clone(),
                            replay,
                            count,
                            snapshot: snapshot.clone(),
                            changing_snapshot,
                        }),
                    ),
                );
                for cycle in 0..4 {
                    snapshot.store(cycle.min(2), std::sync::atomic::Ordering::Relaxed);
                    let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
                    let db: Arc<dyn download::DownloadStore> = if cycle == 1 {
                        let mut failing = FailingMetadataSetDb::without_set_failure(
                            inner.clone(),
                            "injected cached deletion state failure",
                        );
                        failing.fail_source_delete = true;
                        Arc::new(failing)
                    } else {
                        inner.clone()
                    };
                    let builder = make_run_cycle_download_config_builder_with_options(
                        &media,
                        db.clone(),
                        RunCycleDownloadConfigOptions {
                            recent,
                            ..RunCycleDownloadConfigOptions::default()
                        },
                    );
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
                    let held = cycle < 2;
                    let expected_token = if held {
                        "before"
                    } else if changing_snapshot {
                        "after-2"
                    } else {
                        "after"
                    };
                    assert_eq!(
                        inner
                            .get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some(expected_token),
                        "recent={recent:?} replay={replay} changing_snapshot={changing_snapshot} cycle={cycle}"
                    );
                    assert_eq!(
                        inner.sparse_identities("PrimarySync").await.unwrap().len(),
                        if held { count } else { 0 }
                    );
                    assert_eq!(result.stats.state_write_failures > 0, cycle == 1);
                    assert_eq!(result.failed_count > 0, held);
                    assert_eq!(inner.get_summary().await.unwrap().source_deleted, 1);
                    assert_eq!(
                        lookups.load(std::sync::atomic::Ordering::Relaxed),
                        if cycle == 0 { 100 } else { 101 }
                    );
                    assert_eq!(
                        std::fs::read(&sentinel).unwrap(),
                        b"unchanged private media"
                    );
                    if !held {
                        assert_eq!(
                            inner.get_summary().await.unwrap().unresolved_identity_zones,
                            0
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn sparse_deletion_validation_preserves_restored_or_unproven_sources() {
    use crate::state::SparseSourceId;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ValidationChange {
        Restored,
        Incomplete,
        Cancelled,
    }
    #[derive(Clone, Debug)]
    struct ValidationSession {
        change: ValidationChange,
        cycle: usize,
        lookups: Arc<std::sync::atomic::AtomicUsize>,
        validations: Arc<std::sync::atomic::AtomicUsize>,
        shutdown: CancellationToken,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for ValidationSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body).unwrap();
            if let Some(page) = sparse_fixture_inventory(url, &request, &[]) {
                return Ok(page);
            }
            if url.contains("/changes/zone?") {
                let from = request["zones"][0]["syncToken"].as_str().unwrap();
                let mut records = Vec::new();
                let mut token = format!("after-{}", self.cycle.min(2));
                let mut more = false;
                if from != "before" && self.cycle == 1 {
                    self.validations
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if from != "validation-tail" {
                        token = "validation-tail".into();
                        more = true;
                    } else {
                        match self.change {
                            ValidationChange::Restored => {
                                let mut restored =
                                    crate::test_helpers::sparse_shared_asset_record();
                                restored["recordName"] = serde_json::json!("source-000");
                                records.push(restored);
                            }
                            ValidationChange::Incomplete => {
                                anyhow::bail!("injected validation tail failure")
                            }
                            ValidationChange::Cancelled => self.shutdown.cancel(),
                        }
                    }
                }
                return Ok(
                    serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":token,"moreComing":more,"records":records}]}),
                );
            }
            assert!(url.contains("/records/lookup?"));
            let records: Vec<_> = request["records"].as_array().unwrap().iter().map(|record| {
                self.lookups.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if self.change == ValidationChange::Restored && self.cycle == 1 && record["recordName"] == "source-000" {
                    let mut restored = crate::test_helpers::sparse_shared_asset_record();
                    restored["recordName"] = record["recordName"].clone();
                    restored
                } else {
                    serde_json::json!({"recordName":record["recordName"],"serverErrorCode":"UNKNOWN_ITEM"})
                }
            }).collect();
            Ok(serde_json::json!({"records":records}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    for recent in [None, Some(10)] {
        for change in [
            ValidationChange::Restored,
            ValidationChange::Incomplete,
            ValidationChange::Cancelled,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let media = directory.path().join("media");
            let database = directory.path().join("state.db");
            {
                let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
                let db = inner.clone() as Arc<dyn download::DownloadStore>;
                seed_run_cycle_metadata_drift_asset(&db, &media).await;
                inner.set_metadata_capture_revision_for_test(
                    "PrimarySync",
                    "master-PrimarySync",
                    state::METADATA_CAPTURE_REVISION,
                );
                let run = db.start_sync_run().await.unwrap();
                db.complete_sync_run(run, &state::SyncRunStats::default())
                    .await
                    .unwrap();
                db.set_metadata("sync_token:PrimarySync", "before")
                    .await
                    .unwrap();
                let evidence = crate::icloud::photos::asset::SparseShareEvidence::from_fields(
                    &crate::test_helpers::sparse_shared_asset_record()["fields"],
                )
                .unwrap()
                .durable_key()
                .unwrap();
                for index in 0..101 {
                    db.observe_sparse_identity(
                        "PrimarySync",
                        &SparseSourceId::new(&format!("source-{index:03}")),
                        &evidence,
                        chrono::Utc::now(),
                    )
                    .await
                    .unwrap();
                }
            }
            let sentinel = media.join("keep.bin");
            std::fs::write(&sentinel, b"unchanged media").unwrap();
            let mut config = make_run_cycle_config();
            config.filters.recent = recent;
            let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let validations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let cycles = if change == ValidationChange::Restored {
                2
            } else {
                4
            };
            for cycle in 0..cycles {
                let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
                let db = inner.clone() as Arc<dyn download::DownloadStore>;
                let shutdown = CancellationToken::new();
                let library = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    make_full_album_with_boxed_session(
                        "PrimarySync",
                        Box::new(ValidationSession {
                            change,
                            cycle,
                            lookups: lookups.clone(),
                            validations: validations.clone(),
                            shutdown: shutdown.clone(),
                        }),
                    ),
                );
                let builder = make_run_cycle_download_config_builder_with_options(
                    &media,
                    db.clone(),
                    RunCycleDownloadConfigOptions {
                        recent,
                        ..RunCycleDownloadConfigOptions::default()
                    },
                );
                let result = run_cycle(
                    &[&library],
                    &config,
                    Some(db.as_ref()),
                    false,
                    &builder,
                    download::DownloadControls::download_hidden(),
                    &shared_session,
                    &shutdown,
                )
                .await
                .unwrap();
                assert_eq!(
                    db.get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(if cycle < 2 { "before" } else { "after-2" }),
                    "{recent:?} {change:?} cycle {cycle}"
                );
                let retained = db.sparse_identities("PrimarySync").await.unwrap();
                assert_eq!(retained.len(), if cycle < 2 { 101 } else { 0 });
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged media");
                assert_eq!(result.stats.state_write_failures, 0);
                if cycle == 0 {
                    assert_eq!(lookups.load(std::sync::atomic::Ordering::Relaxed), 100);
                } else if cycle >= 2 {
                    assert_eq!(result.failed_count, 0);
                    assert_eq!(
                        lookups.load(std::sync::atomic::Ordering::Relaxed),
                        if change == ValidationChange::Incomplete {
                            200
                        } else {
                            101
                        }
                    );
                    assert_eq!(
                        inner.get_summary().await.unwrap().unresolved_identity_zones,
                        0
                    );
                } else {
                    assert_eq!(validations.load(std::sync::atomic::Ordering::Relaxed), 2);
                    match change {
                        ValidationChange::Restored => {
                            let row = retained
                                .iter()
                                .find(|row| row.source.as_str() == "source-000")
                                .unwrap();
                            assert!(row.deletion_checkpoint.is_none());
                            assert!(row.next_retry.is_some());
                            assert_eq!(lookups.load(std::sync::atomic::Ordering::Relaxed), 102);
                            assert!(result.stats.identity_incomplete);
                        }
                        ValidationChange::Incomplete => {
                            assert_eq!(lookups.load(std::sync::atomic::Ordering::Relaxed), 200);
                            assert!(result.stats.identity_incomplete);
                        }
                        ValidationChange::Cancelled => assert!(result.stats.interrupted),
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn sparse_retry_omitted_source_preserves_failed_work_then_recovers_media() {
    use crate::state::{SparseAttemptOutcome, SparseEvidence, SparseIdentityStore, SparseSourceId};
    use base64::Engine as _;
    use chrono::Utc;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Debug)]
    struct RecoveredSession {
        records: serde_json::Value,
        lookups: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for RecoveredSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            if let Some(page) =
                sparse_fixture_inventory(url, &request, self.records.as_array().unwrap())
            {
                return Ok(page);
            }
            if url.contains("/changes/zone?") {
                return Ok(
                    serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"after","moreComing":false,"records":[]}]}),
                );
            }
            assert!(url.contains("/records/lookup?"));
            let request: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
            let mut names: Vec<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|record| record["recordName"].as_str().unwrap())
                .collect();
            names.sort_unstable();
            assert!(names == ["asset-recovered"] || names == ["asset-recovered", "recovered"]);
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(serde_json::json!({"records":self.records}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    for recent in [None, Some(10)] {
        for interrupted in [false, true] {
            let server = crate::start_wiremock_or_skip!();
            let bytes =
                b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
            let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
            Mock::given(method("GET"))
                .and(path("/recovered.jpg"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .expect(1..=2)
                .mount(&server)
                .await;
            let records = full_album_page_with_download(
                "PrimarySync",
                "recovered",
                "after",
                &format!("{}/recovered.jpg", server.uri()),
                bytes.len() as u64,
                &checksum,
            )["records"]
                .clone();
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("state.db");
            let media = dir.path().join("media");
            let source = SparseSourceId::new("asset-recovered");
            let evidence = SparseEvidence::new(
                r#"[1,"unverified-target","SharedSync-absent","private-owner"]"#.into(),
            );
            {
                let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
                seed_run_cycle_metadata_drift_asset(
                    &(db.clone() as Arc<dyn download::DownloadStore>),
                    &media,
                )
                .await;
                db.set_metadata_capture_revision_for_test(
                    "PrimarySync",
                    "master-PrimarySync",
                    state::METADATA_CAPTURE_REVISION,
                );
                let run = db.start_sync_run().await.unwrap();
                db.complete_sync_run(run, &state::SyncRunStats::default())
                    .await
                    .unwrap();
                db.set_metadata("sync_token:PrimarySync", "before")
                    .await
                    .unwrap();
                let row = db
                    .observe_sparse_identity("PrimarySync", &source, &evidence, Utc::now())
                    .await
                    .unwrap();
                db.record_sparse_attempt(
                    &row,
                    SparseAttemptOutcome::Unresolved(evidence.clone()),
                    Utc::now() - chrono::TimeDelta::hours(2),
                )
                .await
                .unwrap();
                // Make this retry due so the injected write/cancellation boundary runs.
                // The saved mapping cannot bypass a future deadline or prove identity.
                db.upsert_asset_master_mapping("PrimarySync", source.as_str(), "recovered")
                    .await
                    .unwrap();
            }
            let sentinel = media.join("keep-private.bin");
            std::fs::write(&sentinel, b"keep these bytes").unwrap();
            let mut config = make_run_cycle_config();
            config.filters.recent = recent;
            let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut completed_requests = 0;
            for cycle in 0..3 {
                let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
                let cancel = CancellationToken::new();
                let db: Arc<dyn download::DownloadStore> = if cycle == 0 {
                    let failing = FailingMetadataSetDb::without_set_failure(
                        inner.clone(),
                        "injected sparse recovery state failure",
                    );
                    Arc::new(if interrupted {
                        failing.with_cancel_on_upsert(cancel.clone())
                    } else {
                        failing.with_upsert_seen_failure()
                    })
                } else {
                    inner.clone()
                };
                let primary = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    make_full_album_with_boxed_session(
                        "PrimarySync",
                        Box::new(RecoveredSession {
                            records: records.clone(),
                            lookups: lookups.clone(),
                        }),
                    ),
                );
                let builder = make_run_cycle_download_config_builder_with_options(
                    &media,
                    db.clone(),
                    RunCycleDownloadConfigOptions {
                        recent,
                        ..RunCycleDownloadConfigOptions::default()
                    },
                );
                let result = run_cycle(
                    &[&primary],
                    &config,
                    Some(db.as_ref()),
                    false,
                    &builder,
                    download::DownloadControls::download_hidden(),
                    &shared_session,
                    &cancel,
                )
                .await
                .unwrap();
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep these bytes");
                assert_eq!(
                    inner
                        .get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(if cycle == 0 { "before" } else { "after" }),
                    "recent={recent:?} interrupted={interrupted} cycle={cycle}"
                );
                assert_eq!(
                    inner.sparse_identities("PrimarySync").await.unwrap().len(),
                    usize::from(cycle == 0)
                );
                assert_eq!(
                    inner.get_summary().await.unwrap().unresolved_identity_zones,
                    u64::from(cycle == 0)
                );
                let requests = server.received_requests().await.unwrap().len();
                if cycle == 1 {
                    completed_requests = requests;
                }
                if cycle == 2 {
                    assert_eq!(requests, completed_requests);
                }
                if cycle == 0 {
                    assert!(result.stats.interrupted || result.stats.state_write_failures > 0);
                } else {
                    assert_eq!(result.failed_count, 0);
                    assert!(!result.stats.identity_incomplete);
                    let row = inner
                        .get_downloaded_page(0, 20)
                        .await
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id.as_ref() == source.as_str())
                        .unwrap();
                    assert_eq!(std::fs::read(row.local_path.unwrap()).unwrap(), bytes);
                    if cycle == 2 {
                        assert_eq!(result.stats.downloaded, 0);
                    }
                }
            }
            // Each due attempt confirms the current child before fetching its pair.
            assert_eq!(lookups.load(std::sync::atomic::Ordering::Relaxed), 4);
            server.verify().await;
        }
    }
}

#[tokio::test]
async fn run_cycle_hidden_invalid_capture_date_preserves_catalogue() {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Epoch is a valid timestamp control, not a missing-date sentinel.
    for invalid in [None, Some(Value::Null), Some(json!(1e100)), Some(json!(0))] {
        let valid_epoch = invalid == Some(json!(0));
        // Fully valid ambiguous families use schema-28 preservation, covered
        // separately. Here the epoch control isolates valid-date acceptance.
        let sibling_cases: &[Option<bool>] = if valid_epoch {
            &[None]
        } else {
            &[None, Some(false), Some(true)]
        };
        for &sibling_hidden in sibling_cases {
            let with_sibling = sibling_hidden.is_some();
            const ZONE: &str = "SharedSync-HIDDEN";
            const MASTER: &str = "master-hidden";
            const CHILD: &str = "asset-master-hidden";
            const TOKEN_KEY: &str = "sync_token:SharedSync-HIDDEN";
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("state.db");
            let media_dir = dir.path().join("media");
            std::fs::create_dir_all(&media_dir).unwrap();
            let media_path = media_dir.join("legacy.jpg");
            let original_bytes = vec![0u8; 1024];
            std::fs::write(&media_path, &original_bytes).unwrap();
            {
                let db = state::SqliteStateDb::open(&db_path).await.unwrap();
                let record = crate::test_helpers::TestAssetRecord::new(MASTER)
                    .library(ZONE)
                    .filename("legacy.jpg")
                    .created_at(chrono::DateTime::from_timestamp(1_600_000_000, 0).unwrap())
                    .added_at(
                        chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap(),
                    )
                    .size(1024)
                    .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                    .build();
                db.upsert_seen(&record).await.unwrap();
                db.mark_downloaded(
                    ZONE,
                    MASTER,
                    "original",
                    &media_path,
                    "local-checksum",
                    None,
                )
                .await
                .unwrap();
                for child in [CHILD] {
                    db.upsert_asset_master_mapping(ZONE, child, MASTER)
                        .await
                        .unwrap();
                }
                db.set_metadata_capture_revision_for_test(ZONE, MASTER, 0);
                db.set_metadata(TOKEN_KEY, "zone-tok-prev").await.unwrap();
                assert!(
                    db.get_legacy_master_state_owners()
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
            let mut page = full_album_page_with_download(
                ZONE,
                MASTER,
                "zone-tok-new",
                "https://p01.icloud-content.com/photo.jpg",
                1024,
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            );
            page["records"][1]["fields"]["addedDate"] =
                json!({"value": RUN_CYCLE_ASSET_DATE_MS + 123, "type": "TIMESTAMP"});
            page["records"][1]["fields"]["isHidden"] = json!({"value": 1, "type": "INT64"});
            page["records"][1]["fields"]["isFavorite"] = json!({"value": 1, "type": "INT64"});
            match &invalid {
                None => {
                    page["records"][1]["fields"]
                        .as_object_mut()
                        .unwrap()
                        .remove("assetDate");
                }
                Some(value) => {
                    page["records"][1]["fields"]["assetDate"] =
                        json!({"value": value, "type": "TIMESTAMP"});
                }
            }
            if with_sibling {
                let mut sibling = page["records"][1].clone();
                sibling["recordName"] = json!("valid-sibling");
                sibling["fields"]["isHidden"]["value"] = json!(i64::from(sibling_hidden.unwrap()));
                sibling["fields"]["assetDate"] =
                    json!({"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"});
                page["records"].as_array_mut().unwrap().push(sibling);
            }
            let provider_records = Arc::new(std::sync::Mutex::new(
                page["records"].as_array().unwrap().clone(),
            ));
            let repair_requests = Arc::new(AtomicUsize::new(0));
            let album = PhotoAlbum::new(
                PhotoAlbumConfig {
                    params: Arc::new(std::collections::HashMap::new()),
                    service_endpoint: Arc::from("https://example.com"),
                    name: Arc::from("Hidden"),
                    list_type: Arc::from("CPLAssetAndMasterHiddenByAssetDate"),
                    obj_type: Arc::from("CPLAssetHiddenByAssetDate"),
                    query_filter: None,
                    page_size: 100,
                    zone_id: Arc::new(json!({"zoneName": ZONE})),
                    retry_config: retry::RetryConfig::default(),
                    container_id: None,
                    cross_zone_sources: Vec::new(),
                },
                Box::new(crate::sync_loop::test_support::MutableCaptureSession {
                    zone: ZONE,
                    cancel_after_lookup: None,
                    records: Arc::clone(&provider_records),
                    repair_requests: Arc::clone(&repair_requests),
                }),
            );
            let lib_state = make_run_cycle_library_state_with_passes(
                ZONE,
                TOKEN_KEY,
                vec![crate::commands::AlbumPass {
                    kind: crate::commands::PassKind::SmartFolder,
                    album,
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                }],
            );
            let config = make_run_cycle_config();
            let mut completed_requests = None;
            for cycle in 0..5 {
                eprintln!(
                    "capture date={invalid:?} sibling_hidden={sibling_hidden:?} cycle={cycle}: seed -> malformed/control -> reopen -> valid+write-failure -> retry -> steady"
                );
                // A real SQLite failure after valid evidence arrives must not
                // commit the catalogue refresh or its capture receipt.
                if cycle == 2 && !with_sibling && !valid_epoch {
                    rusqlite::Connection::open(&db_path).unwrap().execute_batch(
                        "CREATE TRIGGER fail_capture_refresh BEFORE UPDATE OF created_at ON assets BEGIN SELECT RAISE(ABORT, 'injected capture refresh failure'); END;"
                    ).unwrap();
                }
                if cycle == 3 && !with_sibling && !valid_epoch {
                    rusqlite::Connection::open(&db_path)
                        .unwrap()
                        .execute_batch("DROP TRIGGER fail_capture_refresh")
                        .unwrap();
                }
                if cycle == 2 && !valid_epoch {
                    provider_records.lock().unwrap()[1]["fields"]["assetDate"] =
                        json!({"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"});
                }
                let recovered = !with_sibling && (valid_epoch || cycle >= 3);
                // A valid current sibling permits source progress without
                // claiming or reinterpreting the ambiguous historical owner.
                let source_complete = recovered || (with_sibling && cycle >= 2);
                let inner = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
                let db = Arc::clone(&inner) as Arc<dyn download::DownloadStore>;
                let build_config = make_run_cycle_download_config_builder_with_options(
                    &media_dir,
                    Arc::clone(&db),
                    RunCycleDownloadConfigOptions {
                        media: media_without_photo_downloads(),
                        ..RunCycleDownloadConfigOptions::default()
                    },
                );
                let (_session_dir, session) = make_shared_session_for_run_cycle().await;
                let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
                let result = run_cycle(
                    &[&lib_state],
                    &config,
                    Some(db.as_ref()),
                    false,
                    &build_config,
                    download::DownloadControls::download_hidden(),
                    &session,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(
                    result.failed_count > 0,
                    !source_complete,
                    "invalid={invalid:?}, sibling={with_sibling}, cycle={cycle}"
                );
                assert_eq!(result.stats.downloaded, 0);
                if cycle == 2 && !with_sibling && !valid_epoch {
                    assert!(trace.contains_event(|event| event.message()
                        == Some("Failed to refresh downloaded asset metadata")
                        && event.field("library") == Some(ZONE)
                        && event.field("asset_id") == Some(MASTER)
                        && event.field("error").is_some_and(|error| {
                            error.contains("injected capture refresh failure")
                        })));
                    assert!(result.stats.state_write_failures > 0);
                }
                // Current-source catalogue refresh is not historical capture repair.
                assert_eq!(result.stats.metadata_capture_refreshed, 0);
                assert_eq!(
                    result.stats.metadata_capture_remaining,
                    u64::from(!recovered)
                );
                drop(build_config);
                drop(db);
                drop(inner);
                let inner = state::SqliteStateDb::open(&db_path).await.unwrap();
                assert_eq!(
                    inner.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
                    Some(if source_complete {
                        "zone-tok-new"
                    } else {
                        "zone-tok-prev"
                    })
                );
                let rows = inner.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0].created_at,
                    chrono::DateTime::from_timestamp(
                        if recovered {
                            if valid_epoch {
                                0
                            } else {
                                RUN_CYCLE_ASSET_DATE_MS / 1000
                            }
                        } else {
                            1_600_000_000
                        },
                        0
                    )
                    .unwrap()
                );
                assert_eq!(rows[0].metadata.is_hidden, recovered);
                assert_eq!(rows[0].metadata.is_favorite, recovered);
                assert_eq!(
                    rows[0].checksum.as_ref(),
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                );
                assert_eq!(
                    rows[0].added_at.unwrap().timestamp(),
                    RUN_CYCLE_ASSET_DATE_MS / 1000
                );
                let summary = inner.get_summary().await.unwrap();
                assert_eq!(
                    (
                        summary.downloaded,
                        summary.policy_excluded,
                        summary.source_deleted
                    ),
                    (1, 0, 0)
                );
                // The current Hidden smart-folder membership independently marks
                // its grouping dirty. No metadata writer is enabled in this fixture.
                let queued = inner.get_pending_metadata_rewrites(10).await.unwrap();
                let grouping_seen = !with_sibling && (valid_epoch || cycle >= 2);
                assert_eq!(
                    queued.len(),
                    usize::from(grouping_seen),
                    "invalid={invalid:?}, sibling={with_sibling}, cycle={cycle}"
                );
                if grouping_seen {
                    assert_eq!(
                        (
                            &*queued[0].library,
                            &*queued[0].id,
                            queued[0].version_size.as_str()
                        ),
                        (ZONE, MASTER, "original")
                    );
                    assert_eq!(queued[0].local_path.as_deref(), Some(media_path.as_path()));
                    let conn = rusqlite::Connection::open(&db_path).unwrap();
                    let membership: i64 = conn.query_row("SELECT count(*) FROM asset_albums WHERE library=?1 AND asset_id=?2 AND album_name='Hidden' AND source='icloud'", [ZONE, MASTER], |row| row.get(0)).unwrap();
                    assert_eq!(membership, 1);
                    let marker: i64 = conn.query_row("SELECT count(*) FROM assets WHERE library=?1 AND id=?2 AND version_size='original' AND status='downloaded' AND metadata_write_failed_at IS NOT NULL", [ZONE, MASTER], |row| row.get(0)).unwrap();
                    assert_eq!(
                        marker, 1,
                        "the recorded grouping marker remains durable when writers are disabled"
                    );
                }
                assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
                assert_eq!(
                    inner.get_legacy_master_state_owners().await.unwrap(),
                    if !with_sibling && (valid_epoch || cycle >= 2) {
                        std::collections::HashSet::from([(
                            ZONE.to_string(),
                            MASTER.to_string(),
                            CHILD.to_string(),
                        )])
                    } else {
                        std::collections::HashSet::new()
                    }
                );
                let status = inner
                    .get_summary()
                    .await
                    .unwrap()
                    .metadata_capture
                    .into_iter()
                    .find(|status| status.library == ZONE)
                    .unwrap();
                assert_eq!(
                    status.pending_revision,
                    if recovered {
                        None
                    } else {
                        Some(state::METADATA_CAPTURE_REVISION)
                    }
                );
                // Inspect persisted retry evidence independently of cycle counters.
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                let retries: i64 = conn.query_row(
                    "SELECT count(*) FROM metadata_capture_retries WHERE library=?1 AND asset_id=?2",
                    [ZONE, MASTER], |row| row.get(0),
                ).unwrap();
                assert_eq!(
                    retries, 0,
                    "stale historical capture does not schedule a retry"
                );
                let revision: i64 = conn.query_row(
                    "SELECT revision FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2",
                    [ZONE, MASTER], |row| row.get(0),
                ).unwrap();
                assert_eq!(
                    revision,
                    if recovered {
                        state::METADATA_CAPTURE_REVISION
                    } else {
                        0
                    }
                );
                assert_eq!(std::fs::read(&media_path).unwrap(), original_bytes);
                assert_eq!(std::fs::read_dir(&media_dir).unwrap().count(), 1);
                if recovered {
                    let requests = repair_requests.load(Ordering::SeqCst);
                    if let Some(previous) = completed_requests {
                        assert_eq!(requests, previous);
                    }
                    completed_requests = Some(requests);
                }
            }
        }
    }
}

// Historical certification/backfill orchestration tests retired with that normal-sync path.
// Current invariants live in v027_baseline (including configuration_safety),
// native requested-metadata and recorded-operation recovery tests. See
// docs/plans/v027-sync.md for the retained behavior and qualification boundary.

/// Two provider snapshots must not authorize an ordinary sync to rewrite
/// recorded existing media or metadata. New-download, explicit refresh and
/// authenticated recovery writers remain covered by their separate controls.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn stable_provider_two_passes_preserve_recorded_media_and_sidecar() {
    for prior_debt in [false, true] {
        exercise_two_pass_preservation(prior_debt).await;
    }
}

#[cfg(feature = "xmp")]
async fn exercise_two_pass_preservation(prior_debt: bool) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tree(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                out.push(path.strip_prefix(root).unwrap().to_path_buf());
                if entry.file_type().unwrap().is_dir() {
                    walk(root, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    #[derive(Clone, Debug)]
    struct ScopedPassSession {
        records: Vec<serde_json::Value>,
        returned_ranks: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        changes_cursors: Arc<std::sync::Mutex<Vec<String>>>,
        requests: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for ScopedPassSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            self.requests
                .lock()
                .unwrap()
                .push((url.to_owned(), request.clone()));
            if url.contains("/changes/zone?") {
                let zone = &request["zones"][0];
                anyhow::ensure!(
                    zone["zoneID"]["zoneName"] == "PrimarySync",
                    "unexpected changes zone"
                );
                let cursor = zone["syncToken"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("missing changes cursor"))?;
                self.changes_cursors.lock().unwrap().push(cursor.to_owned());
                return Ok(serde_json::json!({"zones":[{
                    "zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                    "records":[], "syncToken":"fixture-zone-history-after",
                    "moreComing":false
                }]}));
            }
            if url.contains("/records/lookup?") {
                let requested = request["records"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("missing lookup records"))?;
                let records: Vec<_> = self
                    .records
                    .iter()
                    .filter(|record| {
                        requested
                            .iter()
                            .any(|item| item["recordName"] == record["recordName"])
                    })
                    .cloned()
                    .collect();
                return Ok(serde_json::json!({"records":records}));
            }
            if let Some(page) = sparse_fixture_inventory(url, &request, &self.records) {
                if url.contains("/records/query?") {
                    let names = page["records"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|record| record["recordName"].as_str().unwrap().to_owned())
                        .collect::<Vec<_>>();
                    if !names.is_empty() {
                        self.returned_ranks.lock().unwrap().push(names);
                    }
                }
                return Ok(page);
            }
            anyhow::bail!("unexpected fixture request: {url}")
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/existing.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path().join("state.db");
    let owner = state::db::account::AccountOwner::authenticated(
        "two-pass@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"two-pass-owner"}})).unwrap(),
    )
    .unwrap();
    let sqlite = Arc::new(
        state::SqliteStateDb::open_owned(&state_path, &owner)
            .await
            .unwrap(),
    );
    let inner: Arc<dyn download::DownloadStore> = sqlite.clone();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let media = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let media_path = download_dir
        .path()
        .join(run_cycle_expected_date_dir())
        .join("photo.jpg");
    std::fs::create_dir_all(media_path.parent().unwrap()).unwrap();
    std::fs::write(&media_path, media).unwrap();
    let sidecar_path = media_path.with_file_name("photo.jpg.xmp");
    let sidecar = br#"<?xml version="1.0"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="2"/></rdf:RDF></x:xmpmeta>"#;
    std::fs::write(&sidecar_path, sidecar).unwrap();
    let media_mtime = std::fs::metadata(&media_path).unwrap().modified().unwrap();
    let sidecar_mtime = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();
    let expected_tree = tree(download_dir.path());
    let local_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(media));
    let mut signature = sha1::Sha1::new();
    signature.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    signature.update(media);
    let mut signed = vec![1];
    signed.extend_from_slice(&signature.finalize());
    let provider_checksum = base64::engine::general_purpose::STANDARD.encode(signed);
    let seed_page = full_album_page_with_download(
        "PrimarySync",
        "master-PrimarySync",
        "zone-tok-new",
        &format!("{}/existing.jpg", server.uri()),
        media.len() as u64,
        &provider_checksum,
    );
    let seed_asset = crate::icloud::photos::PhotoAsset::new(
        seed_page["records"][0].clone(),
        seed_page["records"][1].clone(),
    );
    let metadata = (*seed_asset.metadata_arc(state::VersionSizeKey::Original)).clone();
    let old_metadata_hash = metadata.metadata_hash.clone().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("asset-master-PrimarySync")
        .filename("photo.jpg")
        .added_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
        .created_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
        .checksum(&provider_checksum)
        .size(media.len() as u64)
        .metadata(metadata)
        .build();
    inner.upsert_seen(&record).await.unwrap();
    sqlite
        .upsert_asset_master_mapping(
            "PrimarySync",
            "asset-master-PrimarySync",
            "master-PrimarySync",
        )
        .await
        .unwrap();
    inner
        .mark_verified_download(
            "PrimarySync",
            "asset-master-PrimarySync",
            "original",
            &media_path,
            &local_checksum,
            Some(&local_checksum),
            false,
        )
        .await
        .unwrap();
    if prior_debt {
        inner
            .record_metadata_write_failure("PrimarySync", "asset-master-PrimarySync", "original")
            .await
            .unwrap();
    }
    let marker_key = (
        "PrimarySync".to_string(),
        "asset-master-PrimarySync".to_string(),
        "original".to_string(),
    );
    assert_eq!(
        inner
            .get_metadata_retry_markers()
            .await
            .unwrap()
            .contains(&marker_key),
        prior_debt
    );
    let prior_path_marker: Option<i64> = sqlite.acquire_lock("prior metadata debt").unwrap().query_row(
        "SELECT metadata_write_failed_at FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-master-PrimarySync' AND version_size='original' AND local_path=?1",
        [media_path.to_str().unwrap()], |row| row.get(0)
    ).unwrap();
    assert_eq!(prior_path_marker.is_some(), prior_debt);
    let prior_catalog_marker: Option<i64> = sqlite.acquire_lock("prior catalogue debt").unwrap().query_row(
        "SELECT metadata_write_failed_at FROM assets WHERE library='PrimarySync' AND id='asset-master-PrimarySync' AND version_size='original'",
        [], |row| row.get(0)
    ).unwrap();
    assert_eq!(prior_catalog_marker.is_some(), prior_debt);
    let original_markers = (prior_path_marker, prior_catalog_marker);
    let (source, local, path_provider): (String, String, String) = sqlite.acquire_lock("verified initial source receipt").unwrap().query_row(
        "SELECT source_checksum,local_checksum,provider_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-master-PrimarySync' AND version_size='original' AND local_path=?1",
        [media_path.to_str().unwrap()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).unwrap();
    assert_eq!(
        (source, local, path_provider),
        (
            local_checksum.clone(),
            local_checksum.clone(),
            provider_checksum.clone()
        )
    );

    let seeded = inner.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(seeded.len(), 1);
    assert_eq!(seeded[0].library.as_ref(), "PrimarySync");
    assert_eq!(seeded[0].id.as_ref(), "asset-master-PrimarySync");
    assert_eq!(seeded[0].checksum.as_ref(), provider_checksum.as_str());
    assert_eq!(seeded[0].size_bytes, media.len() as u64);
    assert_eq!(seeded[0].local_path.as_deref(), Some(media_path.as_path()));
    assert_eq!(
        seeded[0].metadata.metadata_hash.as_deref(),
        Some(old_metadata_hash.as_str())
    );
    assert_eq!(
        sqlite
            .get_master_record_name_for_asset("PrimarySync", "asset-master-PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("master-PrimarySync")
    );
    let initial_created_at = seeded[0].created_at;
    let initial_added_at = seeded[0].added_at;
    drop(seeded);

    // TEMP triggers observe real owner transactions and survive generation
    // retirement without changing the concrete store or its selection identity.
    sqlite.acquire_lock("two-pass destination audit").unwrap().execute_batch(
        "CREATE TEMP TABLE observed_destinations(event TEXT, generation TEXT, pass_key TEXT, child TEXT, asset_id TEXT, path TEXT, admitted INTEGER, verified_media INTEGER, verified_metadata INTEGER, grouping_hash TEXT, metadata_hash TEXT, metadata_enabled INTEGER, deadline INTEGER, catalog_hash TEXT, marker INTEGER, path_marker INTEGER);
         CREATE TEMP TABLE observed_catalog(old_hash TEXT, new_hash TEXT, old_marker INTEGER, new_marker INTEGER);
         CREATE TEMP TRIGGER observe_catalog_update AFTER UPDATE OF metadata_hash,metadata_write_failed_at ON assets WHEN NEW.library='PrimarySync' AND NEW.id='asset-master-PrimarySync' BEGIN
           INSERT INTO observed_catalog VALUES(OLD.metadata_hash,NEW.metadata_hash,OLD.metadata_write_failed_at,NEW.metadata_write_failed_at);
         END;
         CREATE TEMP TRIGGER observe_destination_insert AFTER INSERT ON provider_active_destinations BEGIN
           INSERT INTO observed_destinations VALUES('insert', NEW.generation, NEW.pass_key, NEW.child, NEW.asset_id, NEW.path, NEW.admitted, NEW.verified_media, NEW.verified_metadata, NEW.grouping_hash, NEW.metadata_hash,
             (SELECT metadata_enabled FROM provider_active_generations WHERE id=NEW.generation),
             (SELECT next_retry_at FROM provider_active_decisions WHERE generation=NEW.generation AND pass_key=NEW.pass_key AND child=NEW.child),
             (SELECT metadata_hash FROM assets WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size),
             (SELECT metadata_write_failed_at FROM assets WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size),
             (SELECT metadata_write_failed_at FROM asset_metadata_paths WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size AND provider_checksum=NEW.checksum AND local_path=json_extract(NEW.path,'$.path')));
         END;
         CREATE TEMP TRIGGER observe_destination_update AFTER UPDATE ON provider_active_destinations BEGIN
           INSERT INTO observed_destinations VALUES('update', NEW.generation, NEW.pass_key, NEW.child, NEW.asset_id, NEW.path, NEW.admitted, NEW.verified_media, NEW.verified_metadata, NEW.grouping_hash, NEW.metadata_hash,
             (SELECT metadata_enabled FROM provider_active_generations WHERE id=NEW.generation),
             (SELECT next_retry_at FROM provider_active_decisions WHERE generation=NEW.generation AND pass_key=NEW.pass_key AND child=NEW.child),
             (SELECT metadata_hash FROM assets WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size),
             (SELECT metadata_write_failed_at FROM assets WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size),
             (SELECT metadata_write_failed_at FROM asset_metadata_paths WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size AND provider_checksum=NEW.checksum AND local_path=json_extract(NEW.path,'$.path')));
         END;"
    ).unwrap();
    let db: Arc<dyn download::DownloadStore> = sqlite.clone();
    let changes_cursors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let first_ranks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let second_ranks = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pass = |name: &str,
                _favourite: bool,
                returned_ranks: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
                capture_db: Arc<state::SqliteStateDb>| {
        let mut page = full_album_page_with_download(
            "PrimarySync",
            "master-PrimarySync",
            "zone-tok-new",
            &format!("{}/existing.jpg", server.uri()),
            media.len() as u64,
            &provider_checksum,
        );
        page["records"][1]["fields"]["isFavorite"] =
            serde_json::json!({"value": 1, "type":"INT64"});
        let selected_asset = crate::icloud::photos::PhotoAsset::new(
            page["records"][0].clone(),
            page["records"][1].clone(),
        );
        let captured = selected_asset.metadata_arc(state::VersionSizeKey::Original);
        snapshots.lock().unwrap().push((
            name.to_owned(),
            captured.metadata_hash.clone().unwrap(),
            captured.rating,
            captured.title.clone(),
        ));
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.invalid"),
                name: Arc::from(name),
                list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
                obj_type: Arc::from(format!("CPLContainerRelationNotDeletedByAssetDate:{name}")),
                query_filter: Some(Arc::new(
                    serde_json::json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":name}}]),
                )),
                page_size: 100,
                zone_id: Arc::new(
                    serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: retry::RetryConfig::default(),
                container_id: Some(Arc::from(name)),
                cross_zone_sources: Vec::new(),
            },
            Box::new(ScopedPassSession {
                records: page["records"].as_array().unwrap().clone(),
                returned_ranks: returned_ranks.clone(),
                changes_cursors: changes_cursors.clone(),
                requests: provider_requests.clone(),
            }),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(capture_db, owner.clone(), "com"),
            Arc::from("private"),
        );
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }
    };
    let lib_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![
            pass("Favourited", true, first_ranks.clone(), sqlite.clone()),
            pass("Captioned", false, second_ranks.clone(), sqlite.clone()),
        ],
    );
    let actual_snapshots = snapshots.lock().unwrap().clone();
    assert_eq!(actual_snapshots.len(), 2);
    assert_eq!(
        actual_snapshots[0].1, actual_snapshots[1].1,
        "stable provider returns the same metadata in both albums"
    );
    assert!(
        actual_snapshots
            .iter()
            .all(|snapshot| snapshot.1 != old_metadata_hash)
    );
    for pass in &lib_state.plan.passes {
        let (capture, scope, zone) = pass
            .album
            .download_scope()
            .unwrap()
            .expect("authenticated pass scope");
        assert!(
            Arc::ptr_eq(&capture.db, &sqlite),
            "selection and download must use the same concrete owner"
        );
        assert!(
            capture.owner == owner,
            "selection must use the authenticated owner"
        );
        assert_eq!(zone["zoneName"], "PrimarySync");
        assert_eq!(zone["ownerRecordName"], "_defaultOwner");
        assert!(
            pass.album.selection_scope()["container_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        sqlite
            .validate_download_scope(owner.clone(), scope, "PrimarySync".to_owned())
            .await
            .unwrap();
    }
    let expected_passes: std::collections::HashSet<_> = lib_state
        .plan
        .passes
        .iter()
        .map(|pass| {
            format!(
                "{:x}",
                Sha256::digest(
                    serde_json::json!({
                        "kind": format!("{:?}", pass.kind),
                        "scope": pass.album.selection_scope(),
                        "excludes": Vec::<String>::new(),
                    })
                    .to_string()
                    .as_bytes()
                )
            )
        })
        .collect();
    assert_eq!(expected_passes.len(), 2);
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            xmp_sidecar: true,
            concurrent_downloads: Some(2),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    assert!(!config.runtime.refresh_metadata);
    let probe_config = build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    assert!(
        !probe_config.refresh_metadata,
        "automatic drift must not request explicit refresh"
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let sidecar_entries = crate::download::metadata_rewrite::test_sidecar_entries(&media_path);
    run_cycle(
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
    .expect("run cycle");
    assert_eq!(
        *changes_cursors.lock().unwrap(),
        vec!["fixture-zone-history-boundary"],
        "the scoped empty bridge must be consumed"
    );
    assert_eq!(
        sidecar_entries.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "ordinary sync must not invoke the recorded sidecar writer"
    );
    assert_eq!(
        tree(download_dir.path()),
        expected_tree,
        "ordinary sync must not add duplicate paths"
    );
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "recorded media must not be fetched"
    );
    assert_eq!(
        std::fs::read(&media_path).unwrap().as_slice(),
        media.as_slice()
    );
    assert_eq!(
        std::fs::read(&sidecar_path).unwrap().as_slice(),
        sidecar.as_slice()
    );
    assert_eq!(
        std::fs::metadata(&media_path).unwrap().modified().unwrap(),
        media_mtime
    );
    assert_eq!(
        std::fs::metadata(&sidecar_path)
            .unwrap()
            .modified()
            .unwrap(),
        sidecar_mtime
    );

    let expected = full_album_page_with_download(
        "PrimarySync",
        "master-PrimarySync",
        "zone-tok-new",
        &format!("{}/existing.jpg", server.uri()),
        media.len() as u64,
        &provider_checksum,
    );
    let expected_names: Vec<String> = expected["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["recordName"].as_str().unwrap().to_owned())
        .collect();
    for returned in [&first_ranks, &second_ranks] {
        assert!(
            returned
                .lock()
                .unwrap()
                .iter()
                .any(|names| names == &expected_names),
            "both authenticated passes must return the selected child and master"
        );
    }
    let expected_path = crate::state::db::provider_selection::SelectionPath::from_path(&media_path)
        .key()
        .unwrap();
    let audit = {
        let conn = sqlite
            .acquire_lock("preserved selected destination evidence")
            .unwrap();
        let mut query = conn.prepare("SELECT DISTINCT pass_key, child, asset_id, path, verified_media, metadata_hash FROM observed_destinations WHERE admitted=1").unwrap();
        query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let passes: std::collections::HashSet<_> = audit
        .iter()
        .filter(|(_, child, asset, path, _, _)| {
            child == &expected_names[1]
                && asset == "asset-master-PrimarySync"
                && path == &expected_path
        })
        .map(|entry| entry.0.clone())
        .collect();
    assert_eq!(
        passes, expected_passes,
        "both passes must identify the recorded destination: {audit:?}"
    );

    let assert_preserved_state = |db: &state::SqliteStateDb| {
        let conn = db.acquire_lock("preserved source and debt").unwrap();
        let row: (String, String, String, String, Option<String>, Option<i64>, Option<i64>) = conn.query_row(
            "SELECT p.local_path,p.provider_checksum,p.source_checksum,p.local_checksum,p.download_checksum,p.metadata_write_failed_at,a.metadata_write_failed_at FROM asset_metadata_paths p JOIN assets a USING(library,id,version_size) WHERE p.library='PrimarySync' AND p.id='asset-master-PrimarySync' AND p.version_size='original' AND p.local_path=?1",
            [media_path.to_str().unwrap()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))
        ).unwrap();
        assert_eq!(row.0, media_path.to_str().unwrap());
        assert_eq!(row.1, provider_checksum);
        assert_eq!(row.2, local_checksum);
        assert_eq!(row.3, local_checksum);
        assert_eq!(row.4.as_deref(), Some(local_checksum.as_str()));
        if prior_debt {
            assert!(
                row.5.is_some() && row.6.is_some(),
                "prior path and catalogue debt must remain visible"
            );
        }
        (row.5, row.6)
    };
    let retained_markers = assert_preserved_state(&sqlite);
    if prior_debt {
        assert_eq!(
            retained_markers, original_markers,
            "ordinary sync must preserve the exact original path and catalogue debt markers"
        );
    }
    let markers = inner.get_metadata_retry_markers().await.unwrap();
    if prior_debt {
        assert!(
            markers.contains(&marker_key),
            "ordinary sync must not silently retire prior metadata debt"
        );
    }
    let rows = inner.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
    assert_eq!(rows[0].checksum.as_ref(), provider_checksum.as_str());
    assert_eq!(rows[0].created_at, initial_created_at);
    assert_eq!(rows[0].added_at, initial_added_at);
    let winning_hash = rows[0]
        .metadata
        .metadata_hash
        .as_deref()
        .expect("winning metadata hash");
    assert!(
        audit.iter().any(
            |(pass, child, asset, path, verified_media, selected_hash)| expected_passes
                .contains(pass)
                && child == &expected_names[1]
                && asset == "asset-master-PrimarySync"
                && path == &expected_path
                && *verified_media == 1
                && selected_hash == winning_hash
        ),
        "the admitted winning destination must reach verified media reuse: {audit:?}"
    );
    let row_metadata = rows[0].metadata.clone();
    let catalog_metadata = serde_json::to_value(&row_metadata).unwrap();
    // Compare every compact receipt column, including optional trust fields.
    let compact_receipts = |db: &state::SqliteStateDb| -> Vec<String> {
        let conn = db.acquire_lock("stable provider compact receipts").unwrap();
        let mut stmt = conn.prepare(
            "SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,trust_receipt,selected_metadata_hash,generation_id) FROM download_receipts WHERE library='PrimarySync' AND child='asset-master-PrimarySync' AND master='master-PrimarySync' AND version_size='original' ORDER BY id"
        ).unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let original_receipts = compact_receipts(&sqlite);
    assert_eq!(
        original_receipts.len(),
        1,
        "winning scoped compact receipt must exist"
    );
    let receipt: serde_json::Value = serde_json::from_str(&original_receipts[0]).unwrap();
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    let expected_scope =
        crate::icloud::photos::inbox::ShadowCapture::new(sqlite.clone(), owner.clone(), "com")
            .scope(
                "private",
                &serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            )
            .unwrap();
    assert_eq!(receipt[1], account_key);
    assert_eq!(receipt[2], provider_key);
    assert_eq!(receipt[3], expected_scope);
    assert_eq!(receipt[4], "PrimarySync");
    assert_eq!(receipt[5], "asset-master-PrimarySync");
    assert_eq!(receipt[6], "master-PrimarySync");
    assert_eq!(receipt[7], "asset-master-PrimarySync");
    let raw_passes = receipt[8].as_str().unwrap();
    let receipt_passes: Vec<String> = if raw_passes.starts_with('[') {
        serde_json::from_str(raw_passes).unwrap()
    } else {
        assert!(!raw_passes.trim().is_empty());
        vec![raw_passes.to_owned()]
    };
    assert!(!receipt_passes.is_empty());
    assert!(receipt_passes.iter().all(|pass| !pass.trim().is_empty()));
    assert!(receipt_passes.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        audit
            .iter()
            .any(
                |(pass, child, asset, path, verified, hash)| expected_passes.contains(pass)
                    && receipt_passes.contains(pass)
                    && child == "asset-master-PrimarySync"
                    && asset == "asset-master-PrimarySync"
                    && path == &expected_path
                    && *verified == 1
                    && hash == winning_hash
            ),
        "compact receipt must include the proven winning pass"
    );
    assert!(
        receipt_passes
            .iter()
            .all(|pass| expected_passes.contains(pass))
    );
    assert_eq!(receipt[9], "original");
    assert_eq!(receipt[10], provider_checksum);
    assert_eq!(receipt[11], media.len());
    assert_eq!(receipt[12], expected_path);
    assert_eq!(receipt[13], local_checksum);
    assert_eq!(receipt[14], local_checksum);
    assert!(!receipt[0].as_str().unwrap().is_empty());
    assert!(!receipt[15].as_str().unwrap().is_empty());
    assert!(receipt[16].is_null() && receipt[17].is_null() && receipt[18].is_null());
    drop(rows);
    drop(probe_config);
    drop(build_download_config);
    drop(lib_state);
    drop(db);
    drop(inner);
    assert_eq!(
        Arc::strong_count(&sqlite),
        1,
        "all first-cycle owner, capture and config handles must be released"
    );
    assert_eq!(tree(download_dir.path()), expected_tree);
    let closed_owner = Arc::downgrade(&sqlite);
    drop(sqlite);
    assert!(
        closed_owner.upgrade().is_none(),
        "the first SQLite owner must be dropped before reopen"
    );

    let reopened = Arc::new(
        state::SqliteStateDb::open_owned(&state_path, &owner)
            .await
            .unwrap(),
    );
    assert_eq!(assert_preserved_state(&reopened), retained_markers);
    if prior_debt {
        assert_eq!(
            assert_preserved_state(&reopened),
            original_markers,
            "closed-owner reopen must preserve exact original debt markers"
        );
    }
    if prior_debt {
        assert!(
            reopened
                .get_metadata_retry_markers()
                .await
                .unwrap()
                .contains(&marker_key)
        );
    }
    assert_eq!(
        compact_receipts(&reopened),
        original_receipts,
        "complete compact receipt must survive closed-owner reopen"
    );
    let reopened_rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(reopened_rows.len(), 1);
    assert_eq!(
        reopened_rows[0].local_path.as_deref(),
        Some(media_path.as_path())
    );
    assert_eq!(reopened_rows[0].created_at, initial_created_at);
    assert_eq!(reopened_rows[0].added_at, initial_added_at);
    assert_eq!(
        reopened_rows[0].metadata.metadata_hash,
        row_metadata.metadata_hash
    );
    assert_eq!(
        std::fs::read(&media_path).unwrap().as_slice(),
        media.as_slice()
    );
    assert_eq!(
        std::fs::read(&sidecar_path).unwrap().as_slice(),
        sidecar.as_slice()
    );
    assert_eq!(
        std::fs::metadata(&media_path).unwrap().modified().unwrap(),
        media_mtime
    );
    assert_eq!(
        std::fs::metadata(&sidecar_path)
            .unwrap()
            .modified()
            .unwrap(),
        sidecar_mtime
    );

    let quiet_requests_before = provider_requests.lock().unwrap().len();
    let quiet_db: Arc<dyn download::DownloadStore> = reopened.clone();
    let quiet_library = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![
            pass("Favourited", true, first_ranks.clone(), reopened.clone()),
            pass("Captioned", false, second_ranks.clone(), reopened.clone()),
        ],
    );
    let quiet_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        quiet_db.clone(),
        RunCycleDownloadConfigOptions {
            xmp_sidecar: true,
            concurrent_downloads: Some(2),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let quiet_result = run_cycle(
        &[&quiet_library],
        &config,
        Some(quiet_db.as_ref()),
        false,
        &quiet_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await;
    quiet_result.expect("quiet reopened cycle");
    {
        let requests = provider_requests.lock().unwrap();
        let quiet = &requests[quiet_requests_before..];
        assert!(
            quiet.iter().any(|(url, _)| url.contains("/changes/zone?")),
            "quiet cycle must consume provider changes"
        );
        assert!(
            quiet.iter().all(|(url, _)| !url.contains("/records/query?")
                && !url.contains("/internal/records/query/batch?")),
            "quiet cycle must not redo rank inventory: {quiet:?}"
        );
    }
    assert_eq!(
        tree(download_dir.path()),
        expected_tree,
        "quiet reopen must not add duplicate paths"
    );
    let quiet_rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(quiet_rows.len(), 1);
    assert_eq!(
        quiet_rows[0].local_path.as_deref(),
        Some(media_path.as_path())
    );
    assert_eq!(quiet_rows[0].created_at, initial_created_at);
    assert_eq!(quiet_rows[0].added_at, initial_added_at);
    assert_eq!(
        serde_json::to_value(&quiet_rows[0].metadata).unwrap(),
        catalog_metadata,
        "quiet cycle must preserve complete catalogue metadata and hash"
    );
    assert_eq!(
        compact_receipts(&reopened),
        original_receipts,
        "quiet cycle must preserve complete compact receipt"
    );
    let quiet_markers = assert_preserved_state(&reopened);
    if prior_debt {
        assert_eq!(
            quiet_markers, original_markers,
            "quiet cycle must preserve exact original debt markers"
        );
    }
    if retained_markers.0.is_some() {
        assert!(
            quiet_markers.0.is_some(),
            "path metadata debt must remain visible"
        );
    }
    if retained_markers.1.is_some() {
        assert!(
            quiet_markers.1.is_some(),
            "catalogue metadata debt must remain visible"
        );
    }
    if prior_debt {
        assert!(
            reopened
                .get_metadata_retry_markers()
                .await
                .unwrap()
                .contains(&marker_key)
        );
    }
    assert_eq!(
        std::fs::read(&media_path).unwrap().as_slice(),
        media.as_slice()
    );
    assert_eq!(
        std::fs::read(&sidecar_path).unwrap().as_slice(),
        sidecar.as_slice()
    );
    assert_eq!(
        std::fs::metadata(&media_path).unwrap().modified().unwrap(),
        media_mtime
    );
    assert_eq!(
        std::fs::metadata(&sidecar_path)
            .unwrap()
            .modified()
            .unwrap(),
        sidecar_mtime
    );
    assert_eq!(
        sidecar_entries.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "quiet reopen must not invoke the recorded sidecar writer"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    eprintln!("PRESERVATION_REACHED prior_debt={prior_debt} quiet_reopen=true");
}

// ---------------------------------------------------------------------------
// Authenticated Phase 2 post-proof.
//
// The same owned full+bridge fixture from Phase 1 now runs through the
// production `MetadataWriterWitness` carrier. One physical Destination attempt
// is expected; the bridge and the outer Ordinary drain suppress the identical
// retained request without a second physical write.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "xmp", unix))]
fn prime_support_callsites() {
    // Tracing callsite interest is process-global. Prime the production support
    // diagnostics under a live layerless subscriber so later capture layers
    // receive the DEBUG phase and checkpoint events.
    tracing::subscriber::with_default(tracing_subscriber::registry(), || {
        crate::support::observe(
            "support_pass_completion_v1",
            serde_json::json!({"component": "initial", "phase": "full"}),
        );
        crate::support::observe(
            "support_checkpoint_v1",
            serde_json::json!({"phase": "probe"}),
        );
    });
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Clone, Debug)]
struct WitnessEvent {
    fields: std::collections::HashMap<String, String>,
}

#[cfg(all(feature = "xmp", unix))]
impl WitnessEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Clone, Default)]
struct WitnessLog {
    events: Arc<std::sync::Mutex<Vec<WitnessEvent>>>,
}

#[cfg(all(feature = "xmp", unix))]
impl WitnessLog {
    fn layer(&self) -> WitnessLayer {
        WitnessLayer {
            events: Arc::clone(&self.events),
        }
    }

    fn events(&self) -> Vec<WitnessEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(all(feature = "xmp", unix))]
struct WitnessLayer {
    events: Arc<std::sync::Mutex<Vec<WitnessEvent>>>,
}

#[cfg(all(feature = "xmp", unix))]
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WitnessLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = WitnessVisitor::default();
        event.record(&mut visitor);
        let interesting = visitor.fields.contains_key("diagnostic")
            || visitor.fields.values().any(|value| {
                value.contains("baseline source revalidation")
                    || value.contains("inventory_delta_bridge_failed")
                    || value.contains("Invalid changes/zone cardinality")
            });
        if interesting {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(WitnessEvent {
                    fields: visitor.fields,
                });
        }
    }
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Default)]
struct WitnessVisitor {
    fields: std::collections::HashMap<String, String>,
}

#[cfg(all(feature = "xmp", unix))]
impl tracing::field::Visit for WitnessVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[cfg(all(feature = "xmp", unix))]
async fn seed_verified_queued_sidecar(
    inner: &Arc<dyn download::DownloadStore>,
    download_dir: &std::path::Path,
) -> String {
    use sha2::{Digest, Sha256};

    seed_run_cycle_metadata_drift_asset(inner, download_dir).await;
    let path = download_dir
        .join(run_cycle_expected_date_dir())
        .join("photo.jpg");
    let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(std::fs::read(&path).unwrap()));
    inner
        .mark_downloaded(
            "PrimarySync",
            "master-PrimarySync",
            "original",
            &path,
            &checksum,
            None,
        )
        .await
        .unwrap();
    checksum
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Clone)]
struct AuthenticatedQueuedSession {
    zone: serde_json::Value,
    predecessor: &'static str,
    successor: &'static str,
    records: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    lookups: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(all(feature = "xmp", unix))]
impl AuthenticatedQueuedSession {
    fn record(&self, label: String) {
        self.requests.lock().unwrap().push(label);
    }
}

#[cfg(all(feature = "xmp", unix))]
#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for AuthenticatedQueuedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.ends_with("/zones/list") {
            self.record("zones/list".into());
            return Ok(serde_json::json!({"zones": [{
                "zoneID": self.zone,
                "syncToken": self.predecessor,
                "moreComing": false,
                "records": []
            }]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            self.record("readiness".into());
            return Ok(serde_json::json!({"records": [{
                "recordName": "index",
                "recordType": "CheckIndexingState",
                "fields": {"state": {"value": "FINISHED"}}
            }]}));
        }
        if url.contains("/internal/records/query/batch") {
            self.record("count".into());
            let count = (self.records.lock().unwrap().len() / 2) as u64;
            return Ok(album_count_response(count));
        }
        if url.contains("/changes/zone?") {
            let has_token = request["zones"][0]["syncToken"].is_string();
            self.record(if has_token {
                "bridge-delta".into()
            } else {
                "inventory-delta".into()
            });
            let records = if has_token {
                Vec::new()
            } else {
                self.records.lock().unwrap().clone()
            };
            return Ok(serde_json::json!({"zones": [{
                "zoneID": self.zone,
                "syncToken": self.successor,
                "moreComing": false,
                "records": records
            }]}));
        }
        if url.contains("/records/lookup?") {
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.record("lookup".into());
            let requested = request["records"].as_array().cloned().unwrap_or_default();
            let selected: Vec<_> = self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|item| item["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(serde_json::json!({"records": selected}));
        }
        if url.contains("/records/query?") {
            let rank = request["query"]["filterBy"].as_array().and_then(|filters| {
                filters
                    .iter()
                    .find(|filter| filter["fieldName"] == "startRank")
                    .and_then(|filter| filter["fieldValue"]["value"].as_u64())
            });
            if let Some(offset) = rank {
                self.record(format!("rank:{offset}"));
                let records = if offset == 0 {
                    self.records.lock().unwrap().clone()
                } else {
                    Vec::new()
                };
                return Ok(serde_json::json!({"records": records, "syncToken": self.successor}));
            }
            self.record("records/query".into());
            return Ok(serde_json::json!({"records": [], "syncToken": self.successor}));
        }
        anyhow::bail!("authenticated queued fixture: unexpected endpoint {url}")
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[cfg(all(feature = "xmp", unix))]
struct HeldPathSnapshot {
    path: std::path::PathBuf,
    held: std::fs::File,
    identity: (u64, u64),
    mode: u32,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    sha256: Option<[u8; 32]>,
    parent: std::fs::File,
    parent_identity: (u64, u64),
    parent_mtime: (i64, i64),
    parent_ctime: (i64, i64),
}

#[cfg(all(feature = "xmp", unix))]
impl HeldPathSnapshot {
    fn hold(path: &std::path::Path, hash_contents: bool) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        let held = std::fs::File::open(path).expect("hold snapshot entry");
        let meta = held.metadata().expect("snapshot entry metadata");
        let parent_path = path.parent().expect("snapshot parent");
        let parent = std::fs::File::open(parent_path).expect("hold snapshot parent");
        let parent_meta = parent.metadata().expect("snapshot parent metadata");
        let sha256 = hash_contents.then(|| {
            use sha2::Digest as _;
            let bytes = std::fs::read(path).expect("snapshot contents");
            <[u8; 32]>::from(sha2::Sha256::digest(&bytes))
        });
        Self {
            path: path.to_path_buf(),
            identity: (meta.dev(), meta.ino()),
            mode: meta.mode(),
            size: meta.len(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            sha256,
            held,
            parent,
            parent_identity: (parent_meta.dev(), parent_meta.ino()),
            parent_mtime: (parent_meta.mtime(), parent_meta.mtime_nsec()),
            parent_ctime: (parent_meta.ctime(), parent_meta.ctime_nsec()),
        }
    }

    fn unchanged(&self) -> bool {
        use sha2::Digest as _;
        use std::os::unix::fs::MetadataExt as _;
        let Ok(meta) = std::fs::symlink_metadata(&self.path) else {
            return false;
        };
        let Ok(held_meta) = self.held.metadata() else {
            return false;
        };
        let Ok(parent_held) = self.parent.metadata() else {
            return false;
        };
        let Ok(parent_meta) = std::fs::metadata(self.path.parent().unwrap()) else {
            return false;
        };
        let identity_matches = (meta.dev(), meta.ino()) == self.identity
            && (held_meta.dev(), held_meta.ino()) == self.identity
            && (parent_held.dev(), parent_held.ino()) == self.parent_identity
            && (parent_meta.dev(), parent_meta.ino()) == self.parent_identity;
        let content_matches = self.sha256.is_none_or(|expected| {
            std::fs::read(&self.path)
                .ok()
                .is_some_and(|bytes| <[u8; 32]>::from(sha2::Sha256::digest(&bytes)) == expected)
        });
        identity_matches
            && meta.mode() == self.mode
            && meta.len() == self.size
            && (meta.mtime(), meta.mtime_nsec()) == self.mtime
            && (meta.ctime(), meta.ctime_nsec()) == self.ctime
            && (parent_meta.mtime(), parent_meta.mtime_nsec()) == self.parent_mtime
            && (parent_meta.ctime(), parent_meta.ctime_nsec()) == self.parent_ctime
            && content_matches
    }

    /// The entry itself stays byte- and identity-identical. The parent
    /// directory is allowed to change when the legitimate sidecar entry is
    /// created after the owned blocker is removed.
    fn entry_unchanged(&self) -> bool {
        use sha2::Digest as _;
        use std::os::unix::fs::MetadataExt as _;
        let Ok(meta) = std::fs::symlink_metadata(&self.path) else {
            return false;
        };
        let Ok(held_meta) = self.held.metadata() else {
            return false;
        };
        let content_matches = self.sha256.is_none_or(|expected| {
            std::fs::read(&self.path)
                .ok()
                .is_some_and(|bytes| <[u8; 32]>::from(sha2::Sha256::digest(&bytes)) == expected)
        });
        (meta.dev(), meta.ino()) == self.identity
            && (held_meta.dev(), held_meta.ino()) == self.identity
            && meta.mode() == self.mode
            && meta.len() == self.size
            && (meta.mtime(), meta.mtime_nsec()) == self.mtime
            && (meta.ctime(), meta.ctime_nsec()) == self.ctime
            && content_matches
    }
}

#[cfg(all(feature = "xmp", unix))]
async fn run_full_enumeration_metadata_cycle_authenticated(
    sqlite: &Arc<state::SqliteStateDb>,
    inner: &Arc<dyn download::DownloadStore>,
    owner: &state::db::account::AccountOwner,
    download_dir: &std::path::Path,
    inventory: Vec<serde_json::Value>,
    refresh: bool,
    witness: Arc<download::MetadataWriterWitness>,
) -> (
    crate::sync_cycle::CycleResult,
    AuthenticatedQueuedSession,
    bool,
) {
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoLibrary;
    use crate::icloud::photos::inbox::ShadowCapture;

    let zone = serde_json::json!({
        "zoneName": "PrimarySync",
        "ownerRecordName": "_defaultOwner"
    });
    let session = AuthenticatedQueuedSession {
        zone: zone.clone(),
        predecessor: "zone-history-predecessor",
        successor: "zone-history-successor",
        records: Arc::new(std::sync::Mutex::new(inventory)),
        requests: Arc::default(),
        lookups: Arc::default(),
    };
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session.clone()),
        Arc::new(zone),
        "private".into(),
        retry::RetryConfig::default(),
    )
    .await
    .expect("authenticated photo library");
    assert!(
        library.is_qualified_private_primary(),
        "fixture must own the full private primary zone"
    );
    let indexing_finished = library.last_indexing_finished();
    library.shadow_capture = Some(ShadowCapture::new(Arc::clone(sqlite), owner.clone(), "com"));
    let mut lib_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    lib_state.library = library;

    let mut config = make_run_cycle_config();
    config.runtime.refresh_metadata = refresh;
    config.metadata.xmp_sidecar = true;
    let base = make_run_cycle_download_config_builder_with_options(
        download_dir,
        Arc::clone(inner),
        RunCycleDownloadConfigOptions {
            media: crate::config::MediaSelection::default(),
            xmp_sidecar: true,
            per_pass_paths: false,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let build_download_config = |mode, excluded, groupings, library| {
        let mut config = (*base(mode, excluded, groupings, library)).clone();
        config.refresh_metadata = refresh;
        Arc::new(config)
    };
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let result = Box::pin(run_cycle_with_witness(
        &[&lib_state],
        &config,
        Some(inner.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
        witness,
        &[crate::sync_cycle::ExpectedSource {
            zone_name: lib_state.zone_name.clone(),
            baseline_scope: lib_state.library.baseline_scope(),
            active_passes: !lib_state.plan.passes.is_empty(),
            skip_authority: crate::sync_cycle::SkipAuthority::Reached,
        }],
    ))
    .await
    .expect("authenticated witnessed run cycle");
    (result, session, indexing_finished)
}

#[cfg(all(feature = "xmp", unix))]
fn assert_auth_route_and_phases(
    session: &AuthenticatedQueuedSession,
    events: &[WitnessEvent],
    require_bridge: bool,
    require_zones_list: bool,
) {
    let requests = session.requests.lock().unwrap().clone();
    assert_eq!(
        requests.first().map(String::as_str),
        Some("readiness"),
        "authenticated readiness is the first provider route: {requests:?}"
    );
    if require_zones_list {
        assert!(requests.iter().any(|request| request == "zones/list"));
    }
    assert!(requests.iter().any(|request| request == "count"));
    if require_bridge {
        assert!(requests.iter().any(|request| request == "bridge-delta"));
    }
    let phases: Vec<(String, String)> = events
        .iter()
        .filter(|event| event.field("diagnostic") == Some("support_pass_completion_v1"))
        .filter_map(|event| event.field("support_fields"))
        .filter_map(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .map(|fields| {
            (
                fields["component"].as_str().unwrap_or("").to_owned(),
                fields["phase"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect::<Vec<(String, String)>>();
    assert!(
        phases
            .iter()
            .any(|(component, phase)| component == "initial" && phase == "full"),
        "full inventory component missing: {phases:?}"
    );
    if require_bridge {
        assert!(
            phases
                .iter()
                .any(|(component, phase)| component == "delta_bridge" && phase == "incremental"),
            "authenticated bridge component missing: {phases:?}"
        );
    }
    assert!(
        !events
            .iter()
            .any(|event| event.fields.values().any(|value| {
                value.contains("inventory_delta_bridge_failed")
                    || value.contains("Invalid changes/zone cardinality")
            })),
        "the bridge must not fall back to a full inventory"
    );
}

#[cfg(all(feature = "xmp", unix))]
fn checkpoint_refused(events: &[WitnessEvent]) -> bool {
    events.iter().any(|event| {
        event
            .fields
            .values()
            .any(|value| value.contains("baseline source revalidation lacks completed proof"))
    })
}

#[cfg(all(feature = "xmp", unix))]
fn physical_request_count(events: &[WitnessEvent]) -> usize {
    events
        .iter()
        .filter(|event| event.field("diagnostic") == Some("test_queued_writer_request_v1"))
        .count()
}

#[cfg(all(feature = "xmp", unix))]
fn physical_outcomes(events: &[WitnessEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter(|event| event.field("diagnostic") == Some("test_queued_writer_outcome_v1"))
        .filter_map(|event| {
            event
                .field("test_writer_fields")
                .and_then(|value| serde_json::from_str(value).ok())
        })
        .collect()
}

#[cfg(all(feature = "xmp", unix))]
async fn authenticated_fixture(
    temp: &std::path::Path,
) -> (
    Arc<state::SqliteStateDb>,
    Arc<dyn download::DownloadStore>,
    state::db::account::AccountOwner,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
) {
    let owner = crate::sync_loop::test_support::run_cycle_account_owner();
    let sqlite = Arc::new(
        state::SqliteStateDb::open_owned(&temp.join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let inner: Arc<dyn download::DownloadStore> = sqlite.clone();
    let checksum = seed_verified_queued_sidecar(&inner, temp).await;
    sqlite
        .record_metadata_write_failure("PrimarySync", "master-PrimarySync", "original")
        .await
        .unwrap();
    let media = temp.join(run_cycle_expected_date_dir()).join("photo.jpg");
    let sidecar = media.with_file_name("photo.jpg.xmp");
    std::fs::create_dir(&sidecar).expect("block the sidecar path");
    (sqlite, inner, owner, media, sidecar, checksum)
}

#[cfg(all(feature = "xmp", unix))]
async fn ordinary_pending(
    inner: &Arc<dyn download::DownloadStore>,
) -> Vec<state::db::PendingMetadataRewrite> {
    inner
        .get_pending_metadata_rewrites_page_for_queue(
            state::db::MetadataRewriteQueue::Ordinary,
            None,
            0,
            10,
        )
        .await
        .unwrap()
}

#[cfg(all(feature = "xmp", unix))]
async fn receipts_count(sqlite: &Arc<state::SqliteStateDb>) -> i64 {
    sqlite
        .acquire_lock("phase2 receipts")
        .unwrap()
        .query_row("SELECT COUNT(*) FROM download_receipts", [], |r| r.get(0))
        .unwrap()
}

#[cfg(all(feature = "xmp", unix))]
fn open_fd_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        Some(std::fs::read_dir("/proc/self/fd").ok()?.count())
    }
    #[cfg(target_os = "macos")]
    {
        Some(std::fs::read_dir("/dev/fd").ok()?.count())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckpointSnapshot {
    active: Option<String>,
    baseline: Vec<(String, String)>,
    status: Option<String>,
    recovery: Option<String>,
}

#[cfg(all(feature = "xmp", unix))]
async fn checkpoint_snapshot(
    inner: &Arc<dyn download::DownloadStore>,
    sqlite: &Arc<state::SqliteStateDb>,
) -> CheckpointSnapshot {
    let active = inner.get_metadata("sync_token:PrimarySync").await.unwrap();
    let status = inner.get_metadata("last_checkpoint_status").await.unwrap();
    let recovery = inner.get_metadata("last_recovery_action").await.unwrap();
    let baseline: Vec<(String, String)> = sqlite
        .acquire_lock("qualification checkpoint snapshot")
        .unwrap()
        .prepare(
            "SELECT key, value FROM metadata \
             WHERE key LIKE 'baseline_cursor:v27:%' ORDER BY key",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    CheckpointSnapshot {
        active,
        baseline,
        status,
        recovery,
    }
}

#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn run_cycle_authenticated_eligible_ordinary_sidecar_reports_one_physical_attempt() {
    if crate::test_helpers::isolate_process_fds(
        "sync_loop::runner::tests::metadata::run_cycle_authenticated_eligible_ordinary_sidecar_reports_one_physical_attempt",
    ).await {
        return;
    }
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    prime_support_callsites();
    let temp = tempfile::tempdir().expect("download tempdir").keep();
    let (sqlite, inner, owner, media, sidecar, checksum) = authenticated_fixture(&temp).await;
    let download_dir = temp.clone();
    let media_hold = HeldPathSnapshot::hold(&media, true);
    let sidecar_hold = HeldPathSnapshot::hold(&sidecar, false);
    let attempts = download::metadata_rewrite::test_sidecar_attempts(&media);
    let prior_checkpoint = checkpoint_snapshot(&inner, &sqlite).await;
    eprintln!("PHASE2_CHECKPOINT_PRIOR {prior_checkpoint:?}");
    assert_eq!(prior_checkpoint.active, None);
    assert!(prior_checkpoint.baseline.is_empty());
    assert_eq!(prior_checkpoint.status, None);
    assert_eq!(prior_checkpoint.recovery, None);
    let fds_before = open_fd_count();
    eprintln!("PHASE2_FDS_BEFORE {fds_before:?}");

    // First composed operation: one cycle and its own witness.
    let witness = Arc::new(download::MetadataWriterWitness::default());
    let log = WitnessLog::default();
    let (result, session, indexing_finished) = run_full_enumeration_metadata_cycle_authenticated(
        &sqlite,
        &inner,
        &owner,
        &download_dir,
        Vec::new(),
        true,
        Arc::clone(&witness),
    )
    .with_subscriber(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(log.layer()),
    )
    .await;
    let events = log.events();
    let first_checkpoint = checkpoint_snapshot(&inner, &sqlite).await;
    eprintln!("PHASE2_CHECKPOINT_FIRST {first_checkpoint:?}");
    assert_eq!(
        first_checkpoint.active, None,
        "the active provider checkpoint must not advance"
    );
    assert_eq!(first_checkpoint.status, None);
    assert_eq!(first_checkpoint.recovery, None);
    assert_eq!(first_checkpoint.baseline.len(), 1);
    assert!(
        first_checkpoint
            .baseline
            .iter()
            .all(|(_, value)| value == "zone-history-predecessor"),
        "the baseline cursor must retain the actual prior provider token"
    );
    let pending = ordinary_pending(&inner).await;
    let receipts = receipts_count(&sqlite).await;
    let (calls, matches, unchanged) = witness.test_suppression_counts();
    let outcomes = physical_outcomes(&events);
    eprintln!(
        "PHASE2_FIRST attempts={} exif={} pending={} receipts={} suppress_calls={calls} matches={matches} unchanged={unchanged} physical_requests={}",
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        result.stats.exif_failures,
        pending.len(),
        receipts,
        physical_request_count(&events),
    );
    eprintln!(
        "PHASE2_FIRST_ROUTE {:?}",
        session.requests.lock().unwrap().clone()
    );
    let fds_during = open_fd_count();
    eprintln!("PHASE2_FDS_DURING {fds_during:?}");
    drop(witness);
    let fds_after = open_fd_count();
    eprintln!("PHASE2_FDS_AFTER {fds_after:?}");
    if let (Some(before), Some(during), Some(after)) = (fds_before, fds_during, fds_after) {
        assert!(
            during >= before + 4,
            "the witness must retain its entry handles: before={before} during={during}"
        );
        assert!(
            after <= before,
            "dropping the operation must release every retained entry handle: \
             before={before} after={after}"
        );
    }

    assert!(
        indexing_finished,
        "authenticated readiness must be FINISHED"
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "full+bridge composed operation makes one physical attempt"
    );
    assert_eq!(
        result.stats.exif_failures, 1,
        "one physical failure reported"
    );
    assert_eq!(physical_request_count(&events), 1);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["failed_sidecar_attempt"], "Destination");
    assert_eq!(outcomes[0]["success"], false);
    assert!(
        matches >= 1,
        "the bridge request must match the retained one"
    );
    assert_eq!(
        unchanged, matches,
        "retained media and entry must be unchanged"
    );
    assert_eq!(pending.len(), 1, "the Ordinary debt must survive");
    assert_eq!(
        pending[0].asset.local_checksum.as_deref(),
        Some(checksum.as_str())
    );
    assert_eq!(pending[0].asset.download_checksum, None);
    assert_eq!(pending[0].source_checksum, None);
    assert!(pending[0].selection_receipt.is_none());
    assert!(pending[0].capture_repair_receipt.is_none());
    assert_eq!(receipts, 0);
    assert!(media_hold.unchanged(), "media must remain unchanged");
    assert!(
        sidecar_hold.unchanged(),
        "blocking entry must remain unchanged"
    );
    assert_eq!(std::fs::read(&media).unwrap(), vec![0u8; 1024]);
    assert!(
        checkpoint_refused(&events),
        "missing source proof must refuse"
    );
    assert!(result.stats.state_write_failures >= 1);
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::MetadataBackfill)
    );
    assert_auth_route_and_phases(&session, &events, true, true);

    // Fresh composed operation: a new witness must not inherit suppression.
    let witness2 = Arc::new(download::MetadataWriterWitness::default());
    let log2 = WitnessLog::default();
    let (second, session2, _) = run_full_enumeration_metadata_cycle_authenticated(
        &sqlite,
        &inner,
        &owner,
        &download_dir,
        Vec::new(),
        true,
        Arc::clone(&witness2),
    )
    .with_subscriber(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(log2.layer()),
    )
    .await;
    let events2 = log2.events();
    assert_auth_route_and_phases(&session2, &events2, true, false);
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a fresh composed operation gets a new physical attempt"
    );
    assert_eq!(second.stats.exif_failures, 1);
    assert!(media_hold.unchanged());
    assert!(sidecar_hold.unchanged());
    assert_eq!(ordinary_pending(&inner).await.len(), 1);
    assert!(
        session2
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request == "bridge-delta")
    );
    let second_checkpoint = checkpoint_snapshot(&inner, &sqlite).await;
    eprintln!("PHASE2_CHECKPOINT_SECOND {second_checkpoint:?}");
    assert_eq!(
        second_checkpoint, first_checkpoint,
        "a fresh composed operation must not advance the checkpoint"
    );
    assert!(checkpoint_refused(&events2));
    assert!(second.stats.state_write_failures >= 1);

    // Remove only the owned blocker, then a fresh operation writes the sidecar
    // and retires the Ordinary marker without touching media provenance.
    std::fs::remove_dir(&sidecar).expect("remove the owned blocker directory");
    let witness3 = Arc::new(download::MetadataWriterWitness::default());
    let log3 = WitnessLog::default();
    let (repair, session3, _) = run_full_enumeration_metadata_cycle_authenticated(
        &sqlite,
        &inner,
        &owner,
        &download_dir,
        Vec::new(),
        true,
        Arc::clone(&witness3),
    )
    .with_subscriber(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(log3.layer()),
    )
    .await;
    let events3 = log3.events();
    assert_auth_route_and_phases(&session3, &events3, true, false);
    let repaired_pending = ordinary_pending(&inner).await;
    let repaired_receipts = receipts_count(&sqlite).await;
    let rows = inner.get_downloaded_page(0, 10).await.unwrap();
    eprintln!(
        "PHASE2_REPAIR attempts={} exif={} pending={} receipts={} sidecar_file={}",
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        repair.stats.exif_failures,
        repaired_pending.len(),
        repaired_receipts,
        sidecar.is_file(),
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the repair operation makes exactly one new sidecar write attempt"
    );
    assert_eq!(repair.stats.exif_failures, 0);
    assert!(sidecar.is_file(), "the sidecar must be written");
    assert!(!std::fs::read(&sidecar).unwrap().is_empty());
    assert!(
        repaired_pending.is_empty(),
        "the Ordinary marker must retire"
    );
    assert_eq!(repaired_receipts, 0);
    assert_eq!(std::fs::read(&media).unwrap(), vec![0u8; 1024]);
    assert!(
        media_hold.entry_unchanged(),
        "media identity and bytes stay unchanged"
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].local_checksum.as_deref(), Some(checksum.as_str()));
    assert_eq!(rows[0].download_checksum, None);
    assert!(
        session3
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request == "bridge-delta")
    );
    let repair_checkpoint = checkpoint_snapshot(&inner, &sqlite).await;
    eprintln!("PHASE2_CHECKPOINT_REPAIR {repair_checkpoint:?}");
    assert_eq!(
        repair_checkpoint, first_checkpoint,
        "the sidecar repair must not advance the checkpoint"
    );
    assert!(checkpoint_refused(&events3));
    assert!(repair.stats.state_write_failures >= 1);

    // Quiet fresh operation: no writer attempt.
    let witness4 = Arc::new(download::MetadataWriterWitness::default());
    let quiet_log = WitnessLog::default();
    let (quiet, session4, _) = run_full_enumeration_metadata_cycle_authenticated(
        &sqlite,
        &inner,
        &owner,
        &download_dir,
        Vec::new(),
        false,
        Arc::clone(&witness4),
    )
    .with_subscriber(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(quiet_log.layer()),
    )
    .await;
    let quiet_events = quiet_log.events();
    let quiet_sidecar = std::fs::read(&sidecar).unwrap();
    let quiet_checkpoint = checkpoint_snapshot(&inner, &sqlite).await;
    eprintln!("PHASE2_CHECKPOINT_QUIET {quiet_checkpoint:?}");
    assert_eq!(
        quiet_checkpoint.active, None,
        "quiet must not advance the durable provider checkpoint"
    );
    assert_eq!(quiet_checkpoint.status, None);
    assert_eq!(quiet_checkpoint.recovery, None);
    assert!(
        quiet_checkpoint
            .baseline
            .iter()
            .all(|(_, value)| value == "zone-history-predecessor"),
        "quiet must not persist the provider successor as a baseline"
    );
    eprintln!(
        "PHASE2_QUIET attempts={} exif={} state_write_failures={} refusal={} route={:?}",
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        quiet.stats.exif_failures,
        quiet.stats.state_write_failures,
        checkpoint_refused(&quiet_events),
        session4.requests.lock().unwrap().clone(),
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the quiet operation performs no writer attempt"
    );
    assert_eq!(quiet.stats.exif_failures, 0);
    assert_eq!(std::fs::read(&sidecar).unwrap(), quiet_sidecar);
    assert_eq!(std::fs::read(&media).unwrap(), vec![0u8; 1024]);
    assert!(media_hold.entry_unchanged());
    assert!(ordinary_pending(&inner).await.is_empty());
    // The source-revalidation checkpoint does not acquire proof from the empty
    // inventory, so the quiet operation must still refuse honestly rather than
    // pass a false clean advance.
    assert!(
        quiet.stats.state_write_failures > 0,
        "the empty inventory still lacks completed source proof"
    );
    assert!(
        checkpoint_refused(&quiet_events),
        "quiet must keep the missing-source-proof refusal"
    );
    assert_auth_route_and_phases(&session4, &quiet_events, true, false);
}

#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn runner_authenticated_outer_witness_keeps_source_revalidation_closed() {
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    prime_support_callsites();
    let temp = tempfile::tempdir().expect("download tempdir").keep();
    let (sqlite, inner, owner, media, sidecar, checksum) = authenticated_fixture(&temp).await;
    let download_dir = temp.clone();
    let media_hold = HeldPathSnapshot::hold(&media, true);
    let sidecar_hold = HeldPathSnapshot::hold(&sidecar, false);
    let attempts = download::metadata_rewrite::test_sidecar_attempts(&media);
    let prior_checkpoint = checkpoint_snapshot(&inner, &sqlite).await;
    eprintln!("PHASE2_OUTER_CHECKPOINT_PRIOR {prior_checkpoint:?}");
    assert_eq!(prior_checkpoint.active, None);
    assert!(prior_checkpoint.baseline.is_empty());
    assert_eq!(prior_checkpoint.status, None);
    assert_eq!(prior_checkpoint.recovery, None);
    let witness = Arc::new(download::MetadataWriterWitness::default());
    let log = WitnessLog::default();
    let (result, session, _) = run_full_enumeration_metadata_cycle_authenticated(
        &sqlite,
        &inner,
        &owner,
        &download_dir,
        Vec::new(),
        true,
        Arc::clone(&witness),
    )
    .with_subscriber(
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(log.layer()),
    )
    .await;
    let events = log.events();
    let metadata = crate::config::MetadataConfig {
        xmp_sidecar: true,
        ..crate::config::MetadataConfig::default()
    };
    let tail = download::drain_pending_metadata_rewrites_witnessed(
        inner.as_ref(),
        &download_dir,
        &metadata,
        download::CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        Arc::from(".kei-tmp"),
        &CancellationToken::new(),
        Some(Arc::clone(&witness)),
    )
    .await;
    let pending = ordinary_pending(&inner).await;
    let receipts = receipts_count(&sqlite).await;
    let after_checkpoint = checkpoint_snapshot(&inner, &sqlite).await;
    eprintln!("PHASE2_OUTER_CHECKPOINT_AFTER {after_checkpoint:?}");
    assert_eq!(
        after_checkpoint.active, None,
        "the composed outer operation must not advance the durable checkpoint"
    );
    assert_eq!(after_checkpoint.status, None);
    assert_eq!(after_checkpoint.recovery, None);
    assert!(
        after_checkpoint
            .baseline
            .iter()
            .all(|(_, value)| value == "zone-history-predecessor"),
        "the outer drain must retain the actual prior provider token"
    );
    eprintln!(
        "PHASE2_OUTER attempts={} exif={} new_failures={} witnessed_blocked={} pending={} receipts={}",
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        result.stats.exif_failures,
        tail.new_failures,
        tail.witnessed_blocked,
        pending.len(),
        receipts,
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the outer drain must not make a second physical attempt"
    );
    assert_eq!(result.stats.exif_failures, 1);
    assert_eq!(tail.new_failures, 0, "suppression is not a new failure");
    assert!(
        tail.witnessed_blocked > 0,
        "the outer drain must suppress the already-witnessed blocked attempt"
    );
    assert!(
        !crate::sync_loop::runner::refresh_tail_allows_source_revalidation(&tail),
        "source revalidation must remain closed while witnessed debt is outstanding"
    );
    assert!(
        crate::sync_loop::runner::refresh_tail_allows_source_revalidation(
            &download::RefreshTailOutcome::default()
        ),
        "a clean tail still allows source revalidation"
    );
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].asset.local_checksum.as_deref(),
        Some(checksum.as_str())
    );
    assert_eq!(pending[0].asset.download_checksum, None);
    assert_eq!(pending[0].source_checksum, None);
    assert_eq!(receipts, 0);
    assert!(media_hold.unchanged());
    assert!(sidecar_hold.unchanged());
    assert!(checkpoint_refused(&events));
    assert!(result.stats.state_write_failures >= 1);
    assert_auth_route_and_phases(&session, &events, true, true);
}
