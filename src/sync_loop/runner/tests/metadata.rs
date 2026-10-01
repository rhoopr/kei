use crate::sync_loop::test_support::MetadataSetFailure;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[cfg(feature = "xmp")]
use crate::config;
#[cfg(feature = "xmp")]
use crate::sync_cycle::preload_asset_groupings;
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, run_cycle};
#[cfg(feature = "xmp")]
use crate::sync_loop::test_support::make_named_full_album_with_boxed_session;
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions,
    album_count_response, full_album_page, full_album_page_with_download,
    make_full_album_with_boxed_session, make_full_album_with_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle, make_state_db,
    media_without_photo_downloads, run_cycle_expected_date_dir,
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

#[tokio::test]
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
        evidence: EvidenceCase,
        records: Vec<serde_json::Value>,
        valid: Vec<serde_json::Value>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for IdentitySession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/lookup?") {
                self.lookups
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["zoneID"]["zoneName"], "PrimarySync");
                assert_eq!(
                    body["records"],
                    serde_json::json!([{"recordName":"unresolved-child"}])
                );
                let records = if self.unresolved && self.evidence != EvidenceCase::Ordinary {
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
                } else if self.unresolved {
                    Vec::new()
                } else {
                    vec![
                        serde_json::json!({"recordName":"unresolved-child","serverErrorCode":"UNKNOWN_ITEM"}),
                    ]
                };
                return Ok(serde_json::json!({"records":records}));
            }
            if url.contains("/changes/zone?") {
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
                records.push(child);
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

    for (recent, evidence) in [Some(10), None].into_iter().flat_map(|recent| {
        [
            EvidenceCase::Ordinary,
            EvidenceCase::Sparse,
            EvidenceCase::DeletedDelta,
            EvidenceCase::HardDeletedDelta,
            EvidenceCase::Malformed,
            EvidenceCase::Changed,
            EvidenceCase::MarkerWriteFailure,
        ]
        .map(|evidence| (recent, evidence))
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
            .expect(1)
            .mount(&server)
            .await;
        let valid = full_album_page_with_download(
            "PrimarySync",
            "valid-new",
            "zone-after",
            &format!("{}/valid.jpg", server.uri()),
            bytes.len() as u64,
            &checksum,
        )["records"]
            .as_array()
            .unwrap()
            .clone();
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("state.db");
        let media = dir.path().join("media");
        {
            let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
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
            for zone in ["PrimarySync", "SharedSync-test"] {
                inner
                    .set_metadata(&crate::sync_cycle::sync_token_key(zone), "zone-before")
                    .await
                    .unwrap();
            }
        }
        let before = snapshot_files(&media);
        let mut after_download = None;
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
        for cycle in 0..(5 + usize::from(marker_failure) + usize::from(deletion_delta)) {
            let phase = cycle.saturating_sub(usize::from(marker_failure));
            let fail_marker = marker_failure && cycle == 0;
            let fail_deletion = deletion_delta && phase == 3;
            let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
            if phase == 3 {
                // Simulate the retry deadline without sleeping or changing production time.
                rusqlite::Connection::open(&database).unwrap().execute(
                    "UPDATE unresolved_sparse_identities SET next_retry_at=0 WHERE library='PrimarySync'", []
                ).unwrap();
            }
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
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
            let primary = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                make_full_album_with_boxed_session(
                    "PrimarySync",
                    Box::new(IdentitySession {
                        unresolved: phase < 3,
                        lookups: lookups.clone(),
                        evidence,
                        records: run_cycle_favourited_asset_page()["records"]
                            .as_array()
                            .unwrap()
                            .clone(),
                        valid: valid.clone(),
                    }),
                ),
            );
            let shared = make_run_cycle_library_state(
                "SharedSync-test",
                "sync_token:SharedSync-test",
                "shared-after",
            );
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
            assert_eq!(
                result.stats.identity_incomplete,
                phase < recovery_phase,
                "recent={recent:?} evidence={evidence:?} cycle={cycle}"
            );
            assert_eq!(result.failed_count > 0, phase < recovery_phase);
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
                usize::from(phase != 1 && !deferred && !(phase >= 3 && deletion_delta))
            );
            let retained = inner.sparse_identities("PrimarySync").await.unwrap();
            let stable_link = matches!(
                evidence,
                EvidenceCase::Sparse
                    | EvidenceCase::DeletedDelta
                    | EvidenceCase::HardDeletedDelta
                    | EvidenceCase::Changed
                    | EvidenceCase::MarkerWriteFailure
            );
            assert_eq!(
                retained.len(),
                usize::from(phase < recovery_phase && !fail_marker && stable_link)
            );
            if deferred {
                assert!(retained[0].next_retry.is_some());
            }
            assert_eq!(
                crate::cycle_reporter::classify_cycle(
                    &result.stats,
                    result.failed_count,
                    result.session_expired
                ),
                if phase < recovery_phase {
                    crate::cycle_reporter::CycleStatus::Failed
                } else {
                    crate::cycle_reporter::CycleStatus::Success
                }
            );
            assert_eq!(
                result.stats.state_write_failures > 0,
                fail_marker || fail_deletion
            );
            assert!(
                inner
                    .get_master_record_name_for_asset("PrimarySync", "unresolved-child")
                    .await
                    .unwrap()
                    .is_none()
            );
            let marker_expected = phase < recovery_phase && !fail_marker;
            assert_eq!(
                inner
                    .get_metadata(&state::unresolved_identity_key("PrimarySync"))
                    .await
                    .unwrap()
                    .as_deref(),
                marker_expected.then_some("1")
            );
            let summary = inner.get_summary().await.unwrap();
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
                    .get_metadata("sync_token:PrimarySync")
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
                    .get_metadata("sync_token:SharedSync-test")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("shared-after")
            );
            assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
            let files = snapshot_files(&media);
            for (path, contents) in &before {
                assert_eq!(files.get(path), Some(contents));
            }
            if cycle == 0 {
                assert!(files.values().any(|contents| contents == &bytes));
                after_download = Some(files);
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

#[tokio::test]
async fn run_cycle_hidden_legacy_capture_repair_survives_restart() {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ZONE: &str = "SharedSync-HIDDEN";
    const MASTER: &str = "master-hidden";
    const CHILD: &str = "asset-master-hidden";
    const TOKEN_KEY: &str = "sync_token:SharedSync-HIDDEN";
    #[derive(Clone, Debug)]
    struct HiddenCaptureSession {
        records: Arc<Vec<Value>>,
        repair_requests: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for HiddenCaptureSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            if url.contains("/records/lookup?") {
                self.repair_requests.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"records": [self.records[0]]}));
            }
            if url.contains("/changes/zone?") {
                let records = if request["zones"][0]["syncToken"].is_string() {
                    Vec::new()
                } else {
                    self.repair_requests.fetch_add(1, Ordering::SeqCst);
                    self.records.as_ref().clone()
                };
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-tok-new", "moreComing": false, "records": records
                }]}));
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(1));
            }
            assert!(url.contains("/records/query?"));
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            Ok(json!({"records": records, "syncToken": "zone-tok-new"}))
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

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
            .added_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
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
        Box::new(HiddenCaptureSession {
            records: Arc::new(page["records"].as_array().unwrap().clone()),
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
    for cycle in 0..2 {
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
        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(
            result.stats.metadata_capture_refreshed,
            usize::from(cycle == 0)
        );
        assert_eq!(result.stats.metadata_capture_remaining, 0);
        assert_eq!(
            inner.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
            Some("zone-tok-new")
        );
        let rows = inner.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].metadata.is_hidden && rows[0].metadata.is_favorite);
        assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
        let owners = inner.get_legacy_master_state_owners().await.unwrap();
        assert_eq!(owners.len(), 1);
        assert!(owners.contains(&(ZONE.to_string(), MASTER.to_string(), CHILD.to_string())));
        let status = inner
            .get_summary()
            .await
            .unwrap()
            .metadata_capture
            .into_iter()
            .find(|status| status.library == ZONE)
            .unwrap();
        assert_eq!(status.active_revision, state::METADATA_CAPTURE_REVISION);
        assert_eq!(status.pending_revision, None);
        assert_eq!(status.failed_assets, 0);
        assert_eq!(std::fs::read(&media_path).unwrap(), original_bytes);
        assert_eq!(std::fs::read_dir(&media_dir).unwrap().count(), 1);
        assert_eq!(
            repair_requests.load(Ordering::SeqCst),
            2,
            "restart must not repeat metadata lookup or repair scan"
        );
    }
}

#[tokio::test]
async fn run_cycle_legacy_owner_guard_preserves_dates_and_checkpoint() {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ZONE: &str = "SharedSync-HIDDEN";
    const MASTER: &str = "master-hidden";
    const CHILD: &str = "asset-master-hidden";
    const TOKEN_KEY: &str = "sync_token:SharedSync-HIDDEN";
    #[derive(Clone, Debug)]
    struct HiddenCaptureSession {
        records: Arc<Vec<Value>>,
        repair_requests: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for HiddenCaptureSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            if url.contains("/records/lookup?") {
                self.repair_requests.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"records": [self.records[0]]}));
            }
            if url.contains("/changes/zone?") {
                let records = if request["zones"][0]["syncToken"].is_string() {
                    Vec::new()
                } else {
                    self.repair_requests.fetch_add(1, Ordering::SeqCst);
                    self.records.as_ref().clone()
                };
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-tok-new", "moreComing": false, "records": records
                }]}));
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(1));
            }
            assert!(url.contains("/records/query?"));
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            Ok(json!({"records": records, "syncToken": "zone-tok-new"}))
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    for (legacy_added, fail_retry_write) in [
        (chrono::DateTime::from_timestamp(1_640_995_200, 0), false),
        (chrono::DateTime::from_timestamp(1_514_764_800, 0), false),
        (None, false),
        (chrono::DateTime::from_timestamp(1_640_995_200, 0), true),
    ] {
        let surviving_added = chrono::DateTime::from_timestamp(1_514_764_800, 0).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let media_dir = dir.path().join("media");
        std::fs::create_dir_all(&media_dir).unwrap();
        let media_path = media_dir.join("legacy.jpg");
        let original_bytes = vec![0u8; 1024];
        std::fs::write(&media_path, &original_bytes).unwrap();
        {
            let db = state::SqliteStateDb::open(&db_path).await.unwrap();
            let mut record = crate::test_helpers::TestAssetRecord::new(MASTER)
                .library(ZONE)
                .filename("legacy.jpg")
                .size(1024)
                .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .build();
            record.added_at = legacy_added;
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
            for child in [CHILD, "historical-sibling"] {
                db.upsert_asset_master_mapping(ZONE, child, MASTER)
                    .await
                    .unwrap();
            }
            let mut survivor = record.clone();
            survivor.id = CHILD.into();
            survivor.added_at = Some(surviving_added);
            db.upsert_seen(&survivor).await.unwrap();
            let survivor_path = media_dir.join("survivor.jpg");
            std::fs::write(&survivor_path, &original_bytes).unwrap();
            db.mark_downloaded(
                ZONE,
                CHILD,
                "original",
                &survivor_path,
                "local-checksum",
                None,
            )
            .await
            .unwrap();
            db.set_metadata_capture_revision_for_test(
                ZONE,
                CHILD,
                state::METADATA_CAPTURE_REVISION,
            );
            db.set_metadata_capture_revision_for_test(ZONE, MASTER, 0);
            db.set_metadata(TOKEN_KEY, "zone-tok-prev").await.unwrap();
            assert!(
                db.get_legacy_master_state_owners()
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        if fail_retry_write {
            rusqlite::Connection::open(&db_path).unwrap().execute_batch(
                "CREATE TRIGGER fail_owner_guard_retry BEFORE INSERT ON metadata_capture_retries BEGIN SELECT RAISE(ABORT, 'injected retry failure'); END;"
            ).unwrap();
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
            json!({"value": surviving_added.timestamp_millis(), "type": "TIMESTAMP"});
        page["records"][1]["fields"]["isHidden"] = json!({"value": 1, "type": "INT64"});
        page["records"][1]["fields"]["isFavorite"] = json!({"value": 1, "type": "INT64"});
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
            Box::new(HiddenCaptureSession {
                records: Arc::new(page["records"].as_array().unwrap().clone()),
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
        for cycle in 0..3 {
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
            assert!(result.failed_count > 0);
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(result.stats.metadata_capture_refreshed, 0);
            assert_eq!(result.stats.metadata_capture_remaining, 1);
            assert_eq!(
                result.stats.metadata_capture_failures,
                usize::from(cycle == 0 || (fail_retry_write && cycle == 1))
            );
            assert_eq!(
                inner.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
                Some("zone-tok-prev")
            );
            let rows = inner.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), 2);
            let legacy = rows.iter().find(|row| row.id.as_ref() == MASTER).unwrap();
            assert_eq!(legacy.added_at, legacy_added);
            let rows = [legacy];
            assert!(!rows[0].metadata.is_hidden && !rows[0].metadata.is_favorite);
            assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
            let owners = inner.get_legacy_master_state_owners().await.unwrap();
            assert!(owners.is_empty());
            let status = inner
                .get_summary()
                .await
                .unwrap()
                .metadata_capture
                .into_iter()
                .find(|status| status.library == ZONE)
                .unwrap();
            assert_eq!(status.active_revision, 0);
            assert_eq!(
                status.pending_revision,
                Some(state::METADATA_CAPTURE_REVISION)
            );
            assert_eq!(
                status.failed_assets,
                if fail_retry_write && cycle > 0 { 2 } else { 1 }
            );
            if fail_retry_write && cycle == 0 {
                assert!(result.stats.state_write_failures > 0);
                rusqlite::Connection::open(&db_path)
                    .unwrap()
                    .execute_batch("DROP TRIGGER fail_owner_guard_retry")
                    .unwrap();
            }
            assert_eq!(std::fs::read(&media_path).unwrap(), original_bytes);
            assert_eq!(std::fs::read_dir(&media_dir).unwrap().count(), 2);
            assert_eq!(
                repair_requests.load(Ordering::SeqCst),
                if fail_retry_write && cycle > 0 { 4 } else { 2 },
                "deferred ownership guard must not repeat metadata lookup or repair scan"
            );
        }
    }
}

#[tokio::test]
async fn run_cycle_capture_revision_repair_advances_checkpoint_after_durable_metadata() {
    #[derive(Clone, Debug)]
    struct CaptureRevisionSession {
        records: Arc<Vec<serde_json::Value>>,
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

    let page = run_cycle_favourited_asset_page();
    let records = page["records"]
        .as_array()
        .expect("provider page records")
        .clone();
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(CaptureRevisionSession {
            records: Arc::new(records),
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
    assert_eq!(result.stats.metadata_capture_refreshed, 1);
    assert_eq!(result.stats.metadata_capture_remaining, 0);
    assert!(
        inner.get_downloaded_page(0, 1).await.unwrap()[0]
            .metadata
            .is_favorite,
        "automatic repair must durably store the provider metadata"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-new"),
        "durable capture repair must allow the zone checkpoint to advance"
    );
}

#[tokio::test]
async fn run_cycle_metadata_capture_retry_preserves_durable_checkpoint_until_repaired() {
    #[derive(Clone, Debug)]
    struct RetrySession {
        records: Arc<Vec<serde_json::Value>>,
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for RetrySession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            if url.contains("/records/query/batch?") {
                return Ok(album_count_response(0));
            }
            if url.contains("/records/query?") {
                self.calls.lock().unwrap().push("full");
                return Ok(serde_json::json!({"records": [], "syncToken": "refresh-token-new"}));
            }
            if url.contains("/records/lookup?") {
                self.calls.lock().unwrap().push("lookup");
                let names: Vec<_> = request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|record| record["recordName"].as_str().unwrap())
                    .collect();
                let records: Vec<_> = self
                    .records
                    .iter()
                    .filter(|record| names.contains(&record["recordName"].as_str().unwrap()))
                    .collect();
                return Ok(serde_json::json!({"records": records}));
            }
            assert!(url.contains("/changes/zone?"), "no full album enumeration");
            let delta = request["zones"][0]["syncToken"].is_string();
            self.calls
                .lock()
                .unwrap()
                .push(if delta { "delta" } else { "inventory" });
            let records = if delta {
                Vec::new()
            } else {
                self.records.as_ref().clone()
            };
            Ok(serde_json::json!({"zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "zone-tok-new", "moreComing": false, "records": records
            }]}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    for refresh_deferred in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let download_dir = tempfile::tempdir().unwrap();
        {
            let inner = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
            inner
                .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
                .await
                .unwrap();
            let db = inner.clone() as Arc<dyn download::DownloadStore>;
            seed_run_cycle_metadata_drift_asset(&db, download_dir.path()).await;
            inner.set_metadata_capture_revision_for_test("PrimarySync", "master-PrimarySync", 0);
        }
        let media_path = download_dir
            .path()
            .join(run_cycle_expected_date_dir())
            .join("photo.jpg");
        let original_bytes = std::fs::read(&media_path).unwrap();
        let mut records = run_cycle_favourited_asset_page()["records"]
            .as_array()
            .unwrap()
            .clone();
        let mut sibling = records[1].clone();
        sibling["recordName"] = serde_json::json!("second-child");
        records.push(sibling);
        let records = Arc::new(records);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut config = make_run_cycle_config();
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        for cycle in 0..5 {
            config.runtime.refresh_metadata = refresh_deferred && matches!(cycle, 1 | 2);
            // Reopen the database on every cycle, including the deferred cycle.
            let inner = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
            if cycle == 3 {
                // A controlled authoritative fixture, not a new matcher rule.
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
            let db = inner.clone() as Arc<dyn download::DownloadStore>;
            let album = make_full_album_with_boxed_session(
                "PrimarySync",
                Box::new(RetrySession {
                    records: Arc::clone(&records),
                    calls: Arc::clone(&calls),
                }),
            );
            let lib_state = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                album,
            );
            let build = make_run_cycle_download_config_builder_with_options(
                download_dir.path(),
                Arc::clone(&db),
                RunCycleDownloadConfigOptions {
                    ..RunCycleDownloadConfigOptions::default()
                },
            );
            let refresh_build = |mode, excluded, groupings, library| {
                let original = build(mode, excluded, groupings, library);
                let mut download_config = (*original).clone();
                download_config.refresh_metadata = config.runtime.refresh_metadata;
                Arc::new(download_config)
            };
            calls.lock().unwrap().clear();
            let result = run_cycle(
                &[&lib_state],
                &config,
                Some(db.as_ref()),
                false,
                &refresh_build,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(
                result.stats.metadata_capture_failures,
                usize::from(cycle == 0)
            );
            assert_eq!(
                result.stats.metadata_capture_refreshed,
                usize::from(cycle == 3)
            );
            assert_eq!(result.stats.identity_incomplete, cycle < 3);
            assert_eq!(result.failed_count > 0, cycle < 3);
            assert_eq!(
                inner
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some(if cycle < 3 {
                    "zone-tok-prev"
                } else {
                    "zone-tok-new"
                })
            );
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|&&call| call == "lookup")
                    .count(),
                usize::from(cycle == 0 || cycle == 3)
            );
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|&&call| call == "inventory")
                    .count(),
                usize::from(cycle == 0)
            );
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|&&call| call == "full")
                    .count(),
                if config.runtime.refresh_metadata {
                    usize::try_from(crate::icloud::photos::MAX_EMPTY_PAGE_PROBES).unwrap()
                } else {
                    0
                }
            );
            let capture = inner
                .get_summary()
                .await
                .unwrap()
                .metadata_capture
                .remove(0);
            assert_eq!(capture.unresolved_assets, u64::from(cycle < 3));
            assert_eq!(
                capture.pending_revision,
                if cycle < 3 { Some(1) } else { None }
            );
            assert_eq!(std::fs::read(&media_path).unwrap(), original_bytes);
        }
    }
}

/// Runs one full-enumeration cycle with XMP sidecars enabled over a
/// downloaded asset the provider now reports as a favourite.
#[cfg(feature = "xmp")]
async fn run_full_enumeration_metadata_cycle(
    inner: &Arc<dyn download::DownloadStore>,
    download_dir: &std::path::Path,
    per_pass_paths: bool,
    controls: download::DownloadControls,
) -> crate::sync_cycle::CycleResult {
    let config = make_run_cycle_config();
    let db: Arc<dyn download::DownloadStore> = Arc::clone(inner);
    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(run_cycle_favourited_asset_page()),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir,
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            xmp_sidecar: true,
            per_pass_paths,
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    Box::pin(run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        controls,
        &shared_session,
        &CancellationToken::new(),
    ))
    .await
    .expect("run cycle")
}

/// #707 review: both enumeration branches defer their queue, so a full
/// enumeration drains once. A second drain in the same cycle would retry a
/// failed rewrite immediately and report it twice.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_full_enumeration_reports_a_failed_rewrite_once() {
    for per_pass_paths in [false, true] {
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

        let result = run_full_enumeration_metadata_cycle(
            &inner,
            download_dir.path(),
            per_pass_paths,
            download::DownloadControls::download_hidden(),
        )
        .await;

        assert_eq!(
            result.stats.exif_failures, 1,
            "one failing rewrite must be attempted and reported once (per_pass_paths = {per_pass_paths})"
        );
        assert!(
            !inner
                .get_pending_metadata_rewrites_page(None, 0, 10)
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
    let config = make_run_cycle_config();
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
    let build_download_config = make_run_cycle_download_config_builder_with_options(
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
        1,
        "the passes must share one drain, so a single writer owns the file"
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

/// #707 review: a forwarded download writes the snapshot it was planned
/// from. If a concurrent pass has since refreshed the row and raised a
/// marker, the finaliser must not retire it, or the row keeps the new
/// metadata while the file keeps the old and nothing is left to repair it.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_download_does_not_retire_a_marker_raised_for_newer_metadata() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let config = make_run_cycle_config();
    let inner = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

    // The row stays downloaded but its file is gone, so the media task is
    // forwarded rather than skipped as already on disk.
    let media_path = download_dir
        .path()
        .join(run_cycle_expected_date_dir())
        .join("photo.jpg");
    std::fs::remove_file(&media_path).expect("remove the media file");

    let mut newer = state::AssetMetadata {
        is_favorite: true,
        rating: Some(5),
        ..state::AssetMetadata::default()
    };
    newer.refresh_hash();
    let db: Arc<dyn download::DownloadStore> = Arc::new(
        FailingMetadataSetDb::without_set_failure(
            Arc::clone(&inner),
            "__unused_metadata_message__",
        )
        .with_refresh_on_mark_downloaded(newer),
    );

    let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", "image/jpeg"),
        )
        .mount(&server)
        .await;

    let album = make_full_album_with_session(
        "PrimarySync",
        crate::test_helpers::MockPhotosSession::new()
            .ok(album_count_response(1))
            .ok(full_album_page_with_download(
                "PrimarySync",
                "master-PrimarySync",
                "zone-tok-new",
                &format!("{}/photo.jpg", server.uri()),
                body.len() as u64,
                // The provider reports the stored version, so the media
                // task is forwarded only because the file is missing. A
                // different checksum would make this a new version and
                // return the row to pending, which is a separate flow.
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            )),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
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

    run_cycle(
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

    let row = inner
        .get_downloaded_page(0, 10)
        .await
        .expect("read downloaded rows")
        .into_iter()
        .next()
        .expect("the asset stays downloaded");
    assert_eq!(
        row.metadata.rating,
        Some(5),
        "precondition: the racing pass leaves the newer snapshot on the row"
    );

    let published = row.local_path.as_deref().expect("the download published");
    let mut sidecar_name = published.file_name().expect("a file name").to_os_string();
    sidecar_name.push(".xmp");
    let sidecar = std::fs::read_to_string(published.with_file_name(sidecar_name))
        .expect("sidecar written next to the media file");
    assert!(
        sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
        "the file must end on the snapshot the row holds: {sidecar}"
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

        run_full_enumeration_metadata_cycle(&inner, download_dir.path(), false, controls).await;

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
                .get_pending_metadata_rewrites_page(None, 0, 10)
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
                    assert_eq!(
                        result.session_expired,
                        cycle == 0,
                        "{stage:?} HTTP {status} recent={recent:?} cycle={cycle}"
                    );
                    assert_eq!(
                        inner
                            .get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some(if cycle == 0 {
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
                                    "Backup status: unsafe - unresolved asset identity in 1 provider zone; checkpoint replay is required; last sync was interrupted; 1 enumeration error occurred in the last sync"
                                } else {
                                    "Backup status: unsafe - last sync was interrupted; 1 enumeration error occurred in the last sync"
                                }
                            );
                        } else {
                            assert_eq!(
                                status,
                                "Backup status: safe - last sync completed and no pending or failed assets are recorded"
                            );
                        }
                    }
                    if cycle == 0 {
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
                        if capture_pending {
                            assert_eq!(
                                result.stats.metadata_capture_refreshed,
                                usize::from(cycle == 1)
                            );
                            assert_eq!(result.stats.metadata_capture_remaining, 0);
                            assert!(
                                inner.get_downloaded_page(0, 1).await.unwrap()[0]
                                    .metadata
                                    .is_favorite
                            );
                        }
                    }
                }
                if stage == Stage::Capture {
                    assert_eq!(lookups.load(Ordering::SeqCst), 2);
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
            assert_eq!(names, ["asset-recovered", "recovered"]);
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
                    Utc::now(),
                )
                .await
                .unwrap();
                // Exact source identity bypasses a future deadline. The absent
                // linked zone is never queried and supplies no identity proof.
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
            assert_eq!(lookups.load(std::sync::atomic::Ordering::Relaxed), 2);
            server.verify().await;
        }
    }
}

#[tokio::test]
async fn run_cycle_hidden_invalid_capture_date_preserves_catalogue() {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};

    for invalid in [None, Some(Value::Null), Some(json!(1e100))] {
        for with_sibling in [false, true] {
            const ZONE: &str = "SharedSync-HIDDEN";
            const MASTER: &str = "master-hidden";
            const CHILD: &str = "asset-master-hidden";
            const TOKEN_KEY: &str = "sync_token:SharedSync-HIDDEN";
            #[derive(Clone, Debug)]
            struct HiddenCaptureSession {
                records: Arc<std::sync::Mutex<Vec<Value>>>,
                repair_requests: Arc<AtomicUsize>,
            }
            #[async_trait::async_trait]
            impl PhotosSession for HiddenCaptureSession {
                async fn post(
                    &self,
                    url: &str,
                    body: String,
                    _headers: &[(&str, &str)],
                ) -> anyhow::Result<Value> {
                    let request: Value = serde_json::from_str(&body)?;
                    if url.contains("/records/lookup?") {
                        self.repair_requests.fetch_add(1, Ordering::SeqCst);
                        return Ok(json!({"records": [self.records.lock().unwrap()[0]]}));
                    }
                    if url.contains("/changes/zone?") {
                        let records = if request["zones"][0]["syncToken"].is_string() {
                            Vec::new()
                        } else {
                            self.repair_requests.fetch_add(1, Ordering::SeqCst);
                            self.records.lock().unwrap().clone()
                        };
                        return Ok(json!({"zones": [{
                            "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                            "syncToken": "zone-tok-new", "moreComing": false, "records": records
                        }]}));
                    }
                    if url.contains("/internal/records/query/batch") {
                        return Ok(album_count_response(1));
                    }
                    assert!(url.contains("/records/query?"));
                    let offset = request["query"]["filterBy"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|filter| filter["fieldName"] == "startRank")
                        .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                        .unwrap_or(0);
                    let records = if offset == 0 {
                        self.records.lock().unwrap().clone()
                    } else {
                        Vec::new()
                    };
                    Ok(json!({"records": records, "syncToken": "zone-tok-new"}))
                }
                fn clone_box(&self) -> Box<dyn PhotosSession> {
                    Box::new(self.clone())
                }
            }

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
                Box::new(HiddenCaptureSession {
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
            for cycle in 0..4 {
                if cycle == 2 {
                    provider_records.lock().unwrap()[1]["fields"]["assetDate"] =
                        json!({"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"});
                }
                let recovered = cycle >= 2 && !with_sibling;
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
                    !recovered,
                    "invalid={invalid:?}, sibling={with_sibling}, cycle={cycle}"
                );
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(
                    result.stats.metadata_capture_refreshed,
                    usize::from(cycle == 2 && !with_sibling)
                );
                assert_eq!(
                    result.stats.metadata_capture_remaining,
                    u64::from(!recovered)
                );
                assert_eq!(
                    inner.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
                    Some(if recovered {
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
                            RUN_CYCLE_ASSET_DATE_MS / 1000
                        } else {
                            1_600_000_000
                        },
                        0
                    )
                    .unwrap()
                );
                assert_eq!(rows[0].metadata.is_hidden, recovered);
                assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
                assert_eq!(
                    inner.get_legacy_master_state_owners().await.unwrap().len(),
                    usize::from(recovered)
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

// #765: known provider child identities must not imply an owner for an old
// master-keyed receipt. Exercise production cycles, with only synthetic data.
#[derive(Clone, Copy, Debug)]
enum AmbiguousChildFault {
    None,
    Preserve,
    PreserveConfigChange,
    PreserveIncompleteInventory,
    PreserveCheckpointFailure,
    PreserveCompanion,
    PreservePaginatedHidden,
    PreserveHiddenUnselected,
    PreserveBridgeDebt,
    PreserveCancel,
    PreserveStateWrite,
    PreservePathConflict,
    PathConflict,
    Cancel,
    StateWrite,
}

async fn exercise_ambiguous_child_cycles(fault: AmbiguousChildFault) {
    Box::pin(exercise_legacy_child_cycles(fault, &[2, 3])).await;
}

async fn exercise_legacy_child_cycles(fault: AmbiguousChildFault, child_counts: &[usize]) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Debug)]
    struct ChildSession {
        records: Vec<serde_json::Value>,
        complete: bool,
        bridge_debt: bool,
        paginated: bool,
        omit_hidden: bool,
        incomplete_inventory: bool,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for ChildSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            if self.paginated && url.contains("/changes/zone?") {
                let cursor = request["zones"][0]["syncToken"].as_str();
                if cursor.is_none() || cursor == Some("family-page") {
                    let (records, token, more) = if cursor.is_none() {
                        (&self.records[..2], "family-page", true)
                    } else {
                        (&self.records[2..], "after", false)
                    };
                    return Ok(
                        serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":token,"moreComing":more,"records":records}]}),
                    );
                }
            }

            if self.complete
                && url.contains("/changes/zone?")
                && request["zones"][0]["syncToken"].is_string()
                && !self.bridge_debt
            {
                return Ok(
                    serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"after","moreComing":false,"records":[]}]}),
                );
            }
            if url.contains("/changes/zone?") {
                let mut response = serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"after","moreComing":false,"records":self.records}]});
                if self.incomplete_inventory {
                    response["zones"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("moreComing");
                }
                return Ok(response);
            }
            if url.contains("/records/query/batch?") {
                return Ok(album_count_response(if self.complete {
                    (self.records.len() - 1 - usize::from(self.omit_hidden)) as u64
                } else {
                    0
                }));
            }
            if url.contains("/records/query?") {
                let offset = request["query"]["filterBy"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|filter| filter["fieldName"] == "startRank")
                    .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                    .unwrap_or(0);
                let records = if self.complete && offset == 0 {
                    self.records
                        .iter()
                        .filter(|record| {
                            !self.omit_hidden
                                || record["fields"]["isHidden"]["value"] != serde_json::json!(1)
                        })
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                };
                return Ok(serde_json::json!({"records":records,"syncToken":"after"}));
            }
            assert!(url.contains("/records/lookup?"));
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let names = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    names
                        .iter()
                        .any(|name| name["recordName"] == record["recordName"])
                })
                .collect();
            Ok(serde_json::json!({"records":records}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn rows(database: &std::path::Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let conn = rusqlite::Connection::open(database).unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        let count = stmt.column_count();
        stmt.query_map([], |row| (0..count).map(|index| row.get(index)).collect())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    let preserving = matches!(
        fault,
        AmbiguousChildFault::Preserve
            | AmbiguousChildFault::PreserveConfigChange
            | AmbiguousChildFault::PreserveIncompleteInventory
            | AmbiguousChildFault::PreserveCheckpointFailure
            | AmbiguousChildFault::PreserveCompanion
            | AmbiguousChildFault::PreservePaginatedHidden
            | AmbiguousChildFault::PreserveHiddenUnselected
            | AmbiguousChildFault::PreserveBridgeDebt
            | AmbiguousChildFault::PreserveCancel
            | AmbiguousChildFault::PreserveStateWrite
            | AmbiguousChildFault::PreservePathConflict
    );
    for &children in child_counts {
        for sidecars in [false, true] {
            let server = crate::start_wiremock_or_skip!();
            let bytes =
                b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
            let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
            let motion = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";
            let motion_checksum =
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(motion));
            if matches!(fault, AmbiguousChildFault::PreserveCompanion) {
                Mock::given(method("GET"))
                    .and(path("/child.mov"))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(motion))
                    .mount(&server)
                    .await;
            }

            Mock::given(method("GET"))
                .and(path("/child.jpg"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .mount(&server)
                .await;
            let page = full_album_page_with_download(
                "PrimarySync",
                "legacy-master",
                "after",
                &format!("{}/child.jpg", server.uri()),
                bytes.len() as u64,
                &checksum,
            );
            let mut records = vec![page["records"][0].clone()];
            records[0]["fields"]["filenameEnc"] = serde_json::json!({"value":"photo.jpg"});
            if matches!(fault, AmbiguousChildFault::PreserveCompanion) {
                records[0]["fields"]["resOriginalVidComplRes"] = serde_json::json!({"value":{"downloadURL":format!("{}/child.mov",server.uri()),"size":motion.len(),"fileChecksum":motion_checksum}});
                records[0]["fields"]["resOriginalVidComplFileType"] =
                    serde_json::json!({"value":"com.apple.quicktime-movie"});
            }

            let names: Vec<_> = (0..children).map(|i| format!("child-{i}-unique")).collect();
            for (index, name) in names.iter().enumerate() {
                let mut child = page["records"][1].clone();
                child["recordName"] = serde_json::json!(name);
                child["fields"]["isFavorite"] = serde_json::json!({"value":index % 2});
                if index == children - 1
                    && matches!(
                        fault,
                        AmbiguousChildFault::PreservePaginatedHidden
                            | AmbiguousChildFault::PreserveHiddenUnselected
                    )
                {
                    child["fields"]["isHidden"] = serde_json::json!({"value":1});
                }
                records.push(child);
            }
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("state.db");
            let media = dir.path().join("media");
            let destination = media.join(run_cycle_expected_date_dir());
            std::fs::create_dir_all(&destination).unwrap();
            let legacy_path = destination.join("photo.jpg");
            let legacy_xmp = destination.join("photo.jpg.xmp");
            std::fs::write(&legacy_path, bytes).unwrap();
            std::fs::write(&legacy_xmp, b"legacy sidecar: never adopt or replace").unwrap();
            let legacy_motion = destination.join("photo.MOV");
            let legacy_motion_xmp = destination.join("photo.MOV.xmp");
            if matches!(fault, AmbiguousChildFault::PreserveCompanion) {
                std::fs::write(&legacy_motion, motion).unwrap();
                std::fs::write(&legacy_motion_xmp, b"retained motion sidecar").unwrap();
            }
            let collision = destination.join(format!("photo-{}.jpg", bytes.len()));
            if matches!(
                fault,
                AmbiguousChildFault::PathConflict | AmbiguousChildFault::PreservePathConflict
            ) {
                std::fs::write(&collision, b"unrelated existing file").unwrap();
            }
            {
                let inner = state::SqliteStateDb::open(&database).await.unwrap();
                let date =
                    chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
                let mut metadata = state::AssetMetadata::default();
                metadata.refresh_hash();
                let record = crate::test_helpers::TestAssetRecord::new("legacy-master")
                    .filename("photo.jpg")
                    .created_at(date)
                    .added_at(date)
                    .checksum(&checksum)
                    .size(bytes.len() as u64)
                    .metadata(metadata)
                    .build();
                inner.upsert_seen(&record).await.unwrap();
                inner
                    .mark_downloaded(
                        "PrimarySync",
                        "legacy-master",
                        "original",
                        &legacy_path,
                        "legacy-local-checksum",
                        None,
                    )
                    .await
                    .unwrap();
                if matches!(fault, AmbiguousChildFault::PreserveCompanion) {
                    let record = crate::test_helpers::TestAssetRecord::new("legacy-master")
                        .version_size(state::VersionSizeKey::LiveOriginal)
                        .filename("photo.MOV")
                        .created_at(date)
                        .added_at(date)
                        .checksum(&motion_checksum)
                        .size(motion.len() as u64)
                        .build();
                    inner.upsert_seen(&record).await.unwrap();
                    inner
                        .mark_downloaded(
                            "PrimarySync",
                            "legacy-master",
                            "live_original",
                            &legacy_motion,
                            "retained-local-motion",
                            None,
                        )
                        .await
                        .unwrap();
                }
                inner.set_metadata_capture_revision_for_test("PrimarySync", "legacy-master", 0);
                for name in &names {
                    inner
                        .upsert_asset_master_mapping("PrimarySync", name, "legacy-master")
                        .await
                        .unwrap();
                }
                // Historical membership survives even when a complete current
                // inventory has only one (or no) live child. It is not ownership.
                for missing in children..2 {
                    inner
                        .upsert_asset_master_mapping(
                            "PrimarySync",
                            &format!("historical-absent-{missing}"),
                            "legacy-master",
                        )
                        .await
                        .unwrap();
                }
                inner
                    .set_metadata("sync_token:PrimarySync", "before")
                    .await
                    .unwrap();
                inner
                    .begin_metadata_capture_revision(
                        "PrimarySync",
                        state::METADATA_CAPTURE_REVISION,
                    )
                    .await
                    .unwrap();
                let candidate = inner
                    .get_metadata_capture_candidates("PrimarySync", 1, 1)
                    .await
                    .unwrap()
                    .remove(0);
                assert!(
                    inner
                        .defer_metadata_capture_ambiguity(&candidate, 1)
                        .await
                        .unwrap()
                );
            }
            if matches!(fault, AmbiguousChildFault::PreserveCheckpointFailure) {
                rusqlite::Connection::open(&database).unwrap().execute_batch(
                    "CREATE TRIGGER fail_single_survivor_proof BEFORE INSERT ON unattributed_legacy_proofs BEGIN SELECT RAISE(ABORT,'injected checkpoint receipt failure'); END;"
                ).unwrap();
            }
            let receipt_sql = "SELECT * FROM assets WHERE id='legacy-master'";
            let retry_sql = "SELECT * FROM metadata_capture_retries WHERE asset_id='legacy-master'";
            let revision_sql =
                "SELECT * FROM asset_metadata_capture_revisions WHERE asset_id='legacy-master'";
            let mapping_sql = "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings ORDER BY asset_record_name";
            let receipt = rows(&database, receipt_sql);
            let retry = rows(&database, retry_sql);
            let revision = rows(&database, revision_sql);
            let mappings = rows(&database, mapping_sql);
            let config = make_run_cycle_config();
            let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
            let mut completed_requests = 0;
            let mut completed_outputs = std::collections::HashMap::new();
            for cycle in 0..if matches!(fault, AmbiguousChildFault::PreserveConfigChange) {
                4
            } else {
                3
            } {
                let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
                let cancel = CancellationToken::new();
                let db: Arc<dyn download::DownloadStore> = if cycle == 0
                    && matches!(
                        fault,
                        AmbiguousChildFault::Cancel
                            | AmbiguousChildFault::StateWrite
                            | AmbiguousChildFault::PreserveCancel
                            | AmbiguousChildFault::PreserveStateWrite
                    ) {
                    let failing = FailingMetadataSetDb::without_set_failure(
                        inner.clone(),
                        "injected #765 state failure",
                    );
                    Arc::new(
                        if matches!(
                            fault,
                            AmbiguousChildFault::Cancel | AmbiguousChildFault::PreserveCancel
                        ) {
                            failing.with_cancel_on_upsert(cancel.clone())
                        } else {
                            failing.with_mark_downloaded_failure()
                        },
                    )
                } else {
                    inner.clone()
                };
                let provider = ChildSession {
                    records: records.clone(),
                    complete: preserving,
                    bridge_debt: matches!(fault, AmbiguousChildFault::PreserveBridgeDebt),
                    paginated: matches!(fault, AmbiguousChildFault::PreservePaginatedHidden),
                    omit_hidden: matches!(fault, AmbiguousChildFault::PreserveHiddenUnselected),
                    incomplete_inventory: matches!(
                        fault,
                        AmbiguousChildFault::PreserveIncompleteInventory
                    ),
                };
                let mut primary = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    make_full_album_with_boxed_session("PrimarySync", Box::new(provider.clone())),
                );
                if preserving {
                    primary.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                        Box::new(provider.clone()),
                        "PrimarySync",
                    );
                }
                let base_builder = make_run_cycle_download_config_builder_with_options(
                    &media,
                    db.clone(),
                    RunCycleDownloadConfigOptions {
                        #[cfg(feature = "xmp")]
                        xmp_sidecar: sidecars,
                        concurrent_downloads: Some(1),
                        ..RunCycleDownloadConfigOptions::default()
                    },
                );
                let builder = |mode, excluded, groups, library| {
                    let mut built = base_builder(mode, excluded, groups, library);
                    if matches!(fault, AmbiguousChildFault::PreserveConfigChange) && cycle >= 2 {
                        let current = Arc::make_mut(&mut built);
                        current.folder_structure = "relocated".into();
                        current.folder_structure_albums = Arc::from("relocated");
                        current.folder_structure_smart_folders = Arc::from("relocated");
                    }
                    built
                };
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
                let label = format!(
                    "fault={fault:?} children={children} sidecars={sidecars} cycle={cycle}"
                );
                assert_eq!(
                    std::fs::read(&legacy_path).unwrap(),
                    bytes,
                    "legacy bytes {label}"
                );
                assert_eq!(
                    std::fs::read(&legacy_xmp).unwrap(),
                    b"legacy sidecar: never adopt or replace",
                    "legacy XMP {label}"
                );
                if matches!(
                    fault,
                    AmbiguousChildFault::PathConflict | AmbiguousChildFault::PreservePathConflict
                ) {
                    assert_eq!(
                        std::fs::read(&collision).unwrap(),
                        b"unrelated existing file",
                        "conflict {label}"
                    );
                }
                if matches!(fault, AmbiguousChildFault::PreserveCompanion) {
                    assert_eq!(
                        std::fs::read(&legacy_motion).unwrap(),
                        motion,
                        "original motion {label}"
                    );
                    assert_eq!(
                        std::fs::read(&legacy_motion_xmp).unwrap(),
                        b"retained motion sidecar",
                        "original motion metadata {label}"
                    );
                }
                assert_eq!(rows(&database, receipt_sql), receipt, "receipt {label}");
                assert_eq!(rows(&database, retry_sql), retry, "retry {label}");
                assert_eq!(rows(&database, revision_sql), revision, "revision {label}");
                assert_eq!(
                    rows(&database, mapping_sql),
                    mappings,
                    "mapping history {label}"
                );
                assert!(
                    inner
                        .get_legacy_master_state_owners()
                        .await
                        .unwrap()
                        .is_empty(),
                    "no guessed owner {label}"
                );
                let activated = preserving
                    && children > 0
                    && !matches!(fault, AmbiguousChildFault::PreserveIncompleteInventory)
                    && !(cycle == 0
                        && matches!(fault, AmbiguousChildFault::PreserveCheckpointFailure))
                    && !matches!(fault, AmbiguousChildFault::PreserveHiddenUnselected)
                    && (!matches!(fault, AmbiguousChildFault::PreserveBridgeDebt)
                        || (cfg!(feature = "xmp") && sidecars && cycle > 0))
                    && !(cycle == 0
                        && matches!(
                            fault,
                            AmbiguousChildFault::PreserveCancel
                                | AmbiguousChildFault::PreserveStateWrite
                        ));
                assert_eq!(
                    inner
                        .get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(if activated { "after" } else { "before" }),
                    "checkpoint {label}: {:?}",
                    result.stats
                );
                if activated {
                    assert!(
                        !result.stats.identity_incomplete,
                        "preservation is separate from provider identity {label}"
                    );
                    assert_eq!(
                        inner.legacy_preservations("PrimarySync").await.unwrap()[0]
                            .active_generation,
                        Some(
                            if matches!(fault, AmbiguousChildFault::PreserveConfigChange)
                                && cycle >= 2
                            {
                                2
                            } else {
                                1
                            }
                        ),
                        "active generation {label}"
                    );
                    if cycle >= 2
                        && !(matches!(fault, AmbiguousChildFault::PreserveConfigChange)
                            && cycle == 2)
                    {
                        assert!(
                            result.stats.full_enumeration_reason.is_none(),
                            "unchanged cycle should remain incremental {label}"
                        );
                    }
                    assert_eq!(
                        result.stats.unattributed_legacy_assets, 1,
                        "attribution count {label}"
                    );
                    assert_eq!(
                        result.stats.unattributed_legacy_pending, 0,
                        "current proof count {label}"
                    );
                } else if preserving
                    && (children == 0
                        || matches!(fault, AmbiguousChildFault::PreserveIncompleteInventory))
                {
                    assert!(
                        inner
                            .legacy_preservations("PrimarySync")
                            .await
                            .unwrap()
                            .is_empty(),
                        "no incomplete preparation {label}"
                    );
                } else if preserving {
                    assert_eq!(
                        inner.legacy_preservations("PrimarySync").await.unwrap()[0]
                            .active_generation,
                        None,
                        "prepared hold {label}"
                    );
                } else {
                    assert!(result.stats.identity_incomplete, "hold {label}");
                }
                if cycle == 0
                    && matches!(
                        fault,
                        AmbiguousChildFault::Cancel
                            | AmbiguousChildFault::StateWrite
                            | AmbiguousChildFault::PreserveCancel
                            | AmbiguousChildFault::PreserveStateWrite
                    )
                {
                    assert!(
                        result.stats.interrupted || result.stats.state_write_failures > 0,
                        "fault exercised {label}"
                    );
                } else if matches!(fault, AmbiguousChildFault::PreserveIncompleteInventory) {
                    assert_eq!(result.stats.downloaded, 0, "incomplete inventory {label}");
                    assert_eq!(inner.get_downloaded_page(0, 20).await.unwrap().len(), 1);
                    assert!(result.stats.sync_token_blocked, "incomplete hold {label}");
                } else {
                    let downloaded = inner.get_downloaded_page(0, 20).await.unwrap();
                    let mut paths = std::collections::HashSet::new();
                    let mut outputs = std::collections::HashMap::new();
                    paths.insert(legacy_path.clone());
                    for (index, name) in names.iter().enumerate() {
                        if matches!(fault, AmbiguousChildFault::PreserveHiddenUnselected)
                            && index == names.len() - 1
                        {
                            assert!(!downloaded.iter().any(|row| row.id.as_ref() == name));
                            continue;
                        }
                        let row = downloaded
                            .iter()
                            .find(|row| {
                                row.id.as_ref() == name
                                    && row.version_size == state::VersionSizeKey::Original
                            })
                            .unwrap_or_else(|| panic!("child missing {name} {label}"));
                        let path = row.local_path.as_ref().unwrap();
                        assert!(
                            paths.insert(path.clone()),
                            "child aliases another receipt {name} {label}: {path:?}"
                        );
                        assert_eq!(std::fs::read(path).unwrap(), bytes, "child bytes {label}");
                        outputs.insert(path.clone(), std::fs::read(path).unwrap());
                        if matches!(fault, AmbiguousChildFault::PreserveCompanion) {
                            let companion = downloaded
                                .iter()
                                .find(|row| {
                                    row.id.as_ref() == name
                                        && row.version_size == state::VersionSizeKey::LiveOriginal
                                })
                                .expect("independent motion receipt");
                            let target = companion.local_path.as_ref().unwrap();
                            assert_ne!(target, &legacy_motion);
                            assert!(paths.insert(target.clone()));
                            assert_eq!(std::fs::read(target).unwrap(), motion);
                            outputs.insert(target.clone(), std::fs::read(target).unwrap());
                            #[cfg(feature = "xmp")]
                            if sidecars {
                                let target =
                                    std::path::PathBuf::from(format!("{}.xmp", target.display()));
                                assert!(target.is_file());
                                outputs.insert(target.clone(), std::fs::read(target).unwrap());
                            }
                        }
                        #[cfg(feature = "xmp")]
                        if sidecars {
                            let sidecar = path.with_file_name(format!(
                                "{}.xmp",
                                path.file_name().unwrap().to_str().unwrap()
                            ));
                            assert!(sidecar.is_file(), "child sidecar missing {name} {label}");
                            assert_ne!(sidecar, legacy_xmp, "child sidecar aliases legacy {label}");
                            outputs.insert(sidecar.clone(), std::fs::read(&sidecar).unwrap());
                        }
                    }
                    if cycle == 1 {
                        completed_outputs = outputs;
                    } else if cycle >= 2 {
                        if cycle == 2 && matches!(fault, AmbiguousChildFault::PreserveConfigChange)
                        {
                            for (path, bytes) in &completed_outputs {
                                assert_eq!(
                                    &std::fs::read(path).unwrap(),
                                    bytes,
                                    "prior child copy retained {label}"
                                );
                            }
                            assert!(
                                outputs
                                    .keys()
                                    .all(|path| path.starts_with(media.join("relocated"))),
                                "new current paths {label}"
                            );
                            completed_outputs = outputs;
                        } else {
                            assert_eq!(outputs, completed_outputs, "steady files {label}");
                        }
                    }
                }
                if cycle == 0 && matches!(fault, AmbiguousChildFault::PreserveCheckpointFailure) {
                    assert!(
                        rows(&database, "SELECT * FROM unattributed_legacy_proofs").is_empty(),
                        "failed receipt transaction rolled back {label}"
                    );
                    rusqlite::Connection::open(&database)
                        .unwrap()
                        .execute_batch("DROP TRIGGER fail_single_survivor_proof;")
                        .unwrap();
                }
                let requests = server.received_requests().await.unwrap().len();
                if cycle == 1 && !matches!(fault, AmbiguousChildFault::PreserveIncompleteInventory)
                {
                    assert!(
                        requests
                            >= names.len()
                                - usize::from(matches!(
                                    fault,
                                    AmbiguousChildFault::PreserveHiddenUnselected
                                )),
                        "fresh child downloads {label}"
                    );
                    completed_requests = requests;
                }
                if cycle >= 2 {
                    assert_eq!(requests, completed_requests, "steady network {label}");
                    if !(matches!(fault, AmbiguousChildFault::PreserveConfigChange) && cycle == 2) {
                        assert_eq!(result.stats.downloaded, 0, "steady download {label}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn run_cycle_ambiguous_children_independent() {
    Box::pin(exercise_ambiguous_child_cycles(AmbiguousChildFault::None)).await;
}
#[tokio::test]
async fn run_cycle_ambiguous_children_path_conflict() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PathConflict,
    ))
    .await;
}
#[tokio::test]
async fn run_cycle_ambiguous_children_interrupted() {
    Box::pin(exercise_ambiguous_child_cycles(AmbiguousChildFault::Cancel)).await;
}
#[tokio::test]
async fn run_cycle_ambiguous_children_state_failure() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::StateWrite,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_preserved_independently() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::Preserve,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_bridge_debt() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreserveBridgeDebt,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_preserved_interrupted() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreserveCancel,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_preserved_state_failure() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreserveStateWrite,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_preserved_path_conflict() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreservePathConflict,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_companions() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreserveCompanion,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_paginated_hidden() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreservePaginatedHidden,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_unselected_hidden() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreserveHiddenUnselected,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_ambiguous_children_config_reactivation() {
    Box::pin(exercise_ambiguous_child_cycles(
        AmbiguousChildFault::PreserveConfigChange,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_preserved() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::Preserve,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_hidden_paginated() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreservePaginatedHidden,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_incomplete_inventory_holds() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveIncompleteInventory,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_interruption_recovers() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveCancel,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_current_write_failure_recovers() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveStateWrite,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_checkpoint_failure_is_atomic() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveCheckpointFailure,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_bridge_debt_holds() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveBridgeDebt,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_config_change_revalidates() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveConfigChange,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_zero_children_hold() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::Preserve,
        &[0],
    ))
    .await;
}
