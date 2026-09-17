use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[cfg(feature = "xmp")]
use crate::config;
#[cfg(feature = "xmp")]
use crate::sync_cycle::preload_asset_groupings;
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions,
    album_count_response, full_album_page, full_album_page_with_download,
    make_full_album_with_boxed_session, make_full_album_with_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state_with_album, make_shared_session_for_run_cycle, make_state_db,
    media_without_photo_downloads, run_cycle_expected_date_dir,
};
#[cfg(feature = "xmp")]
use crate::sync_loop::test_support::{
    make_named_full_album_with_boxed_session, make_run_cycle_library_state_with_passes,
};
use crate::{download, state};

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

    run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        controls,
        &shared_session,
        &CancellationToken::new(),
    )
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
