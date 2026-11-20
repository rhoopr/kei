use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[cfg(feature = "xmp")]
use crate::config;
#[cfg(feature = "xmp")]
use crate::sync_cycle::preload_asset_groupings;
use crate::sync_cycle::run_cycle;
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions, album_count_response,
    make_owned_cycle_album, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
    media_without_photo_downloads, open_owned_cycle_db, run_cycle_expected_date_dir,
};
use crate::{download, retry, state};

fn full_album_page_with_download(
    zone: &str,
    master: &str,
    token: &str,
    url: &str,
    size: u64,
    checksum: &str,
) -> serde_json::Value {
    let mut page = crate::sync_loop::test_support::full_album_page_with_download(
        zone, master, token, url, size, checksum,
    );
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        serde_json::json!("_defaultOwner");
    page
}

fn make_owned_metadata_album(
    db: &Arc<state::SqliteStateDb>,
    zone: &str,
    mut page: serde_json::Value,
) -> crate::icloud::photos::PhotoAlbum {
    #[derive(Clone, Debug)]
    struct MetadataSession {
        zone: String,
        page: serde_json::Value,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for MetadataSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let records = self.page["records"].as_array().unwrap();
            if url.contains("/records/lookup?") {
                let requested = request["records"].as_array().unwrap();
                let selected: Vec<_> = records
                    .iter()
                    .filter(|record| {
                        requested
                            .iter()
                            .any(|name| name["recordName"] == record["recordName"])
                    })
                    .collect();
                return Ok(serde_json::json!({"records": selected}));
            }
            if url.contains("/changes/zone?") {
                let changes = if request["zones"][0]["syncToken"] == self.page["syncToken"] {
                    Vec::new()
                } else {
                    records.clone()
                };
                return Ok(serde_json::json!({"zones":[{
                    "zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                    "syncToken":self.page["syncToken"],"moreComing":false,"records":changes
                }]}));
            }
            if url.contains("/records/query/batch?") {
                return Ok(album_count_response(
                    records
                        .iter()
                        .filter(|record| record["recordType"] == "CPLAsset")
                        .count() as u64,
                ));
            }
            assert!(url.contains("/records/query?"));
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            Ok(
                serde_json::json!({"records":if offset == 0 { records.clone() } else { Vec::new() },"syncToken":self.page["syncToken"]}),
            )
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    for record in page["records"].as_array_mut().unwrap() {
        if record["recordType"] == "CPLAsset" && record["fields"]["masterRef"].is_object() {
            record["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                serde_json::json!("_defaultOwner");
        }
    }
    crate::sync_loop::test_support::make_owned_cycle_album(
        db,
        zone,
        if zone.starts_with("SharedSync") {
            "shared"
        } else {
            "private"
        },
        Box::new(MetadataSession {
            zone: zone.to_owned(),
            page,
        }),
    )
}

fn bind_metadata_capture(
    db: &Arc<state::SqliteStateDb>,
    zone: &str,
    database: &str,
    album: &mut crate::icloud::photos::PhotoAlbum,
) {
    let owned = make_owned_cycle_album(
        db,
        zone,
        database,
        Box::new(crate::test_helpers::MockPhotosSession::new()),
    );
    let (capture, _, _) = owned.owned_selection_scope().unwrap().unwrap();
    album.set_shadow_capture(capture, Arc::from(database));
}

async fn repair_historical_capture(
    db: &Arc<state::SqliteStateDb>,
    library: &str,
    passes: &[crate::commands::AlbumPass],
) -> (download::SyncStats, usize) {
    let mut config = download::DownloadConfig::test_default();
    config.library = Arc::from(library);
    config.state_db = Some(db.clone());
    config.repair_metadata_capture_for_test(passes).await
}

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

    let download_dir = tempfile::tempdir().expect("download tempdir");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let album = make_owned_metadata_album(&inner, "PrimarySync", page);
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let db: Arc<dyn download::DownloadStore> = inner;
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
    let server = crate::start_wiremock_or_skip!();
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let inner =
        crate::sync_loop::test_support::open_owned_cycle_db(&download_dir.path().join("state.db"))
            .await;
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let (mut page, media_path) = admit_metadata_child(&inner, download_dir.path(), &server).await;
    let before = std::fs::read(&media_path).unwrap();
    page["syncToken"] = serde_json::json!("zone-tok-new");
    page["records"][1]["recordChangeTag"] = serde_json::json!("malformed-date-edit");
    page["records"][1]["fields"]["assetDate"]["value"] = serde_json::json!("not-a-timestamp");
    let album = make_owned_metadata_album(&inner, "PrimarySync", page);
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
    .expect("run malformed capture-date cycle");

    assert_eq!(result.failed_count, 1);
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
        Some("metadata-baseline"),
        "invalid capture dates must replay from the prior checkpoint"
    );
    let reopened = state::SqliteStateDb::open(&download_dir.path().join("state.db"))
        .await
        .unwrap();
    let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].created_at.timestamp_millis(),
        RUN_CYCLE_ASSET_DATE_MS
    );
    assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
    assert_eq!(std::fs::read(media_path).unwrap(), before);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
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
    let inner = crate::sync_loop::test_support::make_state_db();
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
    Box::pin(exercise_selected_metadata_update(
        SelectedMetadataFault::StateWrite,
        2,
        SelectedMetadataRoute::Incremental,
    ))
    .await;
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectedMetadataFault {
    None,
    StateWrite,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectedMetadataRoute {
    Incremental,
    Full,
}

async fn exercise_selected_metadata_update(
    fault: SelectedMetadataFault,
    pass_count: usize,
    route: SelectedMetadataRoute,
) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

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

    let server = crate::start_wiremock_or_skip!();
    let bytes = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    Mock::given(method("GET"))
        .and(path("/selected.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    std::fs::write(media.join("unrelated.xmp"), b"private sidecar bytes").unwrap();
    let config = make_run_cycle_config();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut original_files = None;
    for cycle in 0..4 {
        let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
        if cycle == 1 && route == SelectedMetadataRoute::Full {
            inner
                .acquire_lock("force full metadata selection")
                .unwrap()
                .execute(
                    "DELETE FROM metadata WHERE key=?1",
                    ["sync_token:PrimarySync"],
                )
                .unwrap();
        }
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let token = if cycle == 0 {
            "metadata-before"
        } else {
            "metadata-after"
        };
        let mut page = full_album_page_with_download(
            "PrimarySync",
            "selected-master",
            token,
            &format!("{}/selected.jpg", server.uri()),
            bytes.len() as u64,
            &checksum,
        );
        page["records"][1]["fields"]["isFavorite"] =
            serde_json::json!({"value":i32::from(cycle > 0),"type":"INT64"});
        page["records"][1]["recordChangeTag"] = serde_json::json!(if cycle == 0 {
            "original"
        } else {
            "favourite-edit"
        });
        let album = make_owned_metadata_album(&inner, "PrimarySync", page);
        let mut library =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        for _ in 1..pass_count {
            let mut pass = library.plan.passes[0].clone();
            pass.kind = crate::commands::PassKind::Album;
            library.plan.passes.push(pass);
        }
        let injected = cycle == 1 && fault == SelectedMetadataFault::StateWrite;
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch("DROP TRIGGER IF EXISTS fail_selected_metadata_update")
            .unwrap();
        if injected {
            conn.execute_batch("CREATE TRIGGER fail_selected_metadata_update BEFORE UPDATE OF is_favorite ON assets
                WHEN OLD.id='asset-selected-master' AND NEW.is_favorite=1
                BEGIN SELECT RAISE(ABORT,'injected selected metadata update failure'); END;").unwrap();
        }
        drop(conn);
        let build = make_run_cycle_download_config_builder(&media, db.clone());
        let result = Box::pin(run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        ))
        .await
        .unwrap();
        drop(build);
        drop(db);
        drop(inner);

        let reopened = state::SqliteStateDb::open(&database).await.unwrap();
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1, "one independently identified selected child");
        assert_eq!(rows[0].id.as_ref(), "asset-selected-master");
        assert_eq!(
            rows[0].metadata.is_favorite,
            cycle > 0 && !injected,
            "selected metadata changes only after its transaction commits"
        );
        assert_eq!(
            std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
            bytes
        );
        let source: Option<String> = rusqlite::Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT source_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-selected-master' AND version_size='original' AND local_path=?1",
                [rows[0].local_path.as_ref().unwrap().to_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            source,
            Some(data_encoding::HEXLOWER.encode(&Sha256::digest(bytes)))
        );
        assert!(
            reopened
                .get_legacy_master_state_owners()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            if injected && route == SelectedMetadataRoute::Full {
                None
            } else {
                Some(if cycle == 0 || injected {
                    "metadata-before"
                } else {
                    "metadata-after"
                })
            }
        );
        if cycle == 1 && route == SelectedMetadataRoute::Full {
            assert_eq!(
                result.stats.full_enumeration_reason,
                Some(download::FullEnumerationReason::NoStoredToken)
            );
        }
        assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
        if injected {
            assert!(
                result.stats.state_write_failures > 0,
                "native metadata SQL fault must execute"
            );
            assert!(result.failed_count > 0);
        } else {
            assert_eq!(result.failed_count, 0);
            assert!(!result.stats.sync_token_blocked);
        }
        let files = snapshot_files(&media);
        if let Some(original) = &original_files {
            assert_eq!(
                &files, original,
                "metadata-only updates preserve every media and sidecar path and byte"
            );
        }
        original_files = Some(files);
    }
    server.verify().await;
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
            let request: serde_json::Value = serde_json::from_str(&body)?;
            if let Some(page) = sparse_fixture_inventory(url, &request, &self.valid) {
                return Ok(page);
            }
            if url.contains("/records/lookup?") {
                let requested = request["records"].as_array().unwrap();
                if requested.iter().all(|name| {
                    self.valid
                        .iter()
                        .any(|record| record["recordName"] == name["recordName"])
                }) {
                    let records: Vec<_> = self
                        .valid
                        .iter()
                        .filter(|record| {
                            requested
                                .iter()
                                .any(|name| name["recordName"] == record["recordName"])
                        })
                        .collect();
                    return Ok(serde_json::json!({"records": records}));
                }
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

    for evidence in [
        EvidenceCase::Ordinary,
        EvidenceCase::Sparse,
        EvidenceCase::DeletedDelta,
        EvidenceCase::HardDeletedDelta,
        EvidenceCase::Malformed,
        EvidenceCase::Changed,
        EvidenceCase::MarkerWriteFailure,
    ] {
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
            let inner = open_owned_cycle_db(&database).await;
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
        let config = make_run_cycle_config();
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
            let inner = open_owned_cycle_db(&database).await;
            if phase == 3 {
                // Simulate the retry deadline without sleeping or changing production time.
                rusqlite::Connection::open(&database).unwrap().execute(
                    "UPDATE unresolved_sparse_identities SET next_retry_at=0 WHERE library='PrimarySync'", []
                ).unwrap();
            }
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let conn = rusqlite::Connection::open(&database).unwrap();
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS fail_identity_marker;
                DROP TRIGGER IF EXISTS fail_identity_retirement;",
            )
            .unwrap();
            if fail_marker {
                conn.execute_batch(&format!("CREATE TRIGGER fail_identity_marker BEFORE INSERT ON metadata
                    WHEN NEW.key='{}' BEGIN SELECT RAISE(ABORT,'injected unresolved marker write failure'); END;",
                    state::unresolved_identity_key("PrimarySync"))).unwrap();
            }
            if fail_deletion {
                conn.execute_batch("CREATE TRIGGER fail_identity_retirement BEFORE DELETE ON unresolved_sparse_identities
                    WHEN OLD.library='PrimarySync' BEGIN SELECT RAISE(ABORT,'injected source deletion write failure'); END;").unwrap();
            }
            drop(conn);
            let db: Arc<dyn download::DownloadStore> = inner.clone();
            let primary = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                make_owned_cycle_album(
                    &inner,
                    "PrimarySync",
                    "private",
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
                    let request = serde_json::from_str(&body)?;
                    if let Some(page) = sparse_fixture_inventory(url, &request, &[]) {
                        return Ok(page);
                    }
                    assert!(url.contains("/changes/zone?"));
                    Ok(
                        serde_json::json!({"zones":[{"zoneID":{"zoneName":"SharedSync-test","ownerRecordName":"_defaultOwner"},"records":[],"syncToken":"shared-after","moreComing":false}]}),
                    )
                }
                fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
                    Box::new(self.clone())
                }
            }
            let shared = make_run_cycle_library_state_with_album(
                "SharedSync-test",
                "sync_token:SharedSync-test",
                make_owned_cycle_album(&inner, "SharedSync-test", "shared", Box::new(QuietShared)),
            );
            let libraries = if phase == 1 {
                vec![&shared]
            } else {
                vec![&primary, &shared]
            };
            let builder = make_run_cycle_download_config_builder_with_options(
                &media,
                db.clone(),
                RunCycleDownloadConfigOptions::default(),
            );
            let result = run_cycle(
                &libraries,
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            // Phase 1 selects only the healthy shared zone. The primary
            // marker still fences the database cursor, not cycle completion.
            let selected_identity_debt = phase != 1 && phase < recovery_phase;
            if phase == 1 && !fail_marker {
                assert!(!result.can_advance_database_checkpoint());
            }
            assert_eq!(
                result.stats.identity_incomplete, selected_identity_debt,
                "evidence={evidence:?} cycle={cycle}"
            );
            assert_eq!(result.failed_count > 0, selected_identity_debt);
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
                usize::from(phase != 1 && !deferred && !(phase >= 3 && deletion_delta)),
                "evidence={evidence:?} cycle={cycle} phase={phase}"
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
                if selected_identity_debt {
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
                "evidence={evidence:?} cycle={cycle}: {}",
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
            assert_eq!(
                result.stats.downloaded,
                usize::from(cycle == 0),
                "evidence={evidence:?} cycle={cycle}: {:?}",
                result.stats
            );
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
/// path does. The single unfiled pass routes the cycle through the streaming
/// producer rather than collecting incremental planning.
#[tokio::test]
async fn run_cycle_single_pass_metadata_refresh_failure_preserves_zone_checkpoint() {
    Box::pin(exercise_selected_metadata_update(
        SelectedMetadataFault::StateWrite,
        1,
        SelectedMetadataRoute::Incremental,
    ))
    .await;
}

/// #707: the single-pass streaming path must advance the zone checkpoint
/// once the refreshed provider metadata is durable, applying the edit
/// without downloading media.
#[tokio::test]
async fn run_cycle_single_pass_metadata_refresh_advances_checkpoint_after_durable_state() {
    Box::pin(exercise_selected_metadata_update(
        SelectedMetadataFault::None,
        1,
        SelectedMetadataRoute::Incremental,
    ))
    .await;
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
                return Ok(json!({"records": records}));
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
        let db = open_owned_cycle_db(&db_path).await;
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
            zone_id: Arc::new(json!({"zoneName": ZONE, "ownerRecordName": "_defaultOwner"})),
            retry_config: retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(HiddenCaptureSession {
            records: Arc::new(page["records"].as_array().unwrap().clone()),
            repair_requests: Arc::clone(&repair_requests),
        }),
    );
    let mut lib_state = make_run_cycle_library_state_with_passes(
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
        let inner = open_owned_cycle_db(&db_path).await;
        bind_metadata_capture(&inner, ZONE, "shared", &mut lib_state.plan.passes[0].album);
        let before_lookups = repair_requests.load(Ordering::SeqCst);
        let (capture, failures) =
            repair_historical_capture(&inner, ZONE, &lib_state.plan.passes).await;
        let capture_lookups = repair_requests.load(Ordering::SeqCst) - before_lookups;
        assert_eq!(failures, 0);
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
            &build_config,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(capture.metadata_capture_refreshed, usize::from(cycle == 0));
        assert_eq!(capture.metadata_capture_remaining, 0);
        assert_eq!(
            result.stats.metadata_capture_refreshed, 0,
            "ordinary sync does not repeat explicit maintenance"
        );
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
            capture_lookups,
            if cycle == 0 { 2 } else { 0 },
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
                return Ok(json!({"records": records}));
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
            let db = open_owned_cycle_db(&db_path).await;
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
                zone_id: Arc::new(json!({"zoneName": ZONE, "ownerRecordName": "_defaultOwner"})),
                retry_config: retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(HiddenCaptureSession {
                records: Arc::new(page["records"].as_array().unwrap().clone()),
                repair_requests: Arc::clone(&repair_requests),
            }),
        );
        let mut lib_state = make_run_cycle_library_state_with_passes(
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
            let inner = open_owned_cycle_db(&db_path).await;
            bind_metadata_capture(&inner, ZONE, "shared", &mut lib_state.plan.passes[0].album);
            let before_lookups = repair_requests.load(Ordering::SeqCst);
            let (capture, _) =
                repair_historical_capture(&inner, ZONE, &lib_state.plan.passes).await;
            let capture_lookups = repair_requests.load(Ordering::SeqCst) - before_lookups;
            assert!(
                capture.sync_token_blocked,
                "explicit repair must retain its ownership refusal"
            );
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
                &build_config,
                download::DownloadControls::download_hidden(),
                &session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(
                result.failed_count, 0,
                "historical ownership refusal cannot veto ordinary excluded-current sync"
            );
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(result.stats.metadata_capture_refreshed, 0);
            assert_eq!(capture.metadata_capture_remaining, 1);
            assert_eq!(
                capture.metadata_capture_failures,
                usize::from(cycle == 0 || (fail_retry_write && cycle == 1))
            );
            assert_eq!(
                inner.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
                Some("zone-tok-new")
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
                assert!(capture.state_write_failures > 0);
                rusqlite::Connection::open(&db_path)
                    .unwrap()
                    .execute_batch("DROP TRIGGER fail_owner_guard_retry")
                    .unwrap();
            }
            assert_eq!(std::fs::read(&media_path).unwrap(), original_bytes);
            assert_eq!(std::fs::read_dir(&media_dir).unwrap().count(), 2);
            assert_eq!(
                capture_lookups,
                if cycle == 0 || (fail_retry_write && cycle == 1) {
                    2
                } else {
                    0
                },
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
            Ok(serde_json::json!({"records": [], "syncToken": "zone-tok-new"}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    let config = make_run_cycle_config();
    let state_dir = tempfile::tempdir().unwrap();
    let inner = open_owned_cycle_db(&state_dir.path().join("state.db")).await;
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
    let album = make_owned_cycle_album(
        &inner,
        "PrimarySync",
        "private",
        Box::new(CaptureRevisionSession {
            records: Arc::new(records),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let (capture, failures) =
        repair_historical_capture(&inner, "PrimarySync", &lib_state.plan.passes).await;
    assert_eq!(failures, 0);
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
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(capture.metadata_capture_refreshed, 1);
    assert_eq!(result.stats.metadata_capture_refreshed, 0);
    assert_eq!(result.stats.metadata_capture_remaining, 0);
    assert!(
        inner.get_downloaded_page(0, 1).await.unwrap()[0]
            .metadata
            .is_favorite,
        "explicit repair must durably store the provider metadata"
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
            let inner = open_owned_cycle_db(&db_path).await;
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
            let inner = open_owned_cycle_db(&db_path).await;
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
            let album = make_owned_cycle_album(
                &inner,
                "PrimarySync",
                "private",
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
            let (repair, _) =
                repair_historical_capture(&inner, "PrimarySync", &lib_state.plan.passes).await;
            let result = run_cycle(
                &[&lib_state],
                &config,
                Some(db.as_ref()),
                &refresh_build,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(repair.metadata_capture_failures, usize::from(cycle == 0));
            assert_eq!(repair.metadata_capture_refreshed, usize::from(cycle == 3));
            assert_eq!(repair.identity_incomplete, cycle < 3);
            assert_eq!(
                result.failed_count > 0,
                config.runtime.refresh_metadata && cycle < 3
            );
            assert_eq!(
                inner
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("zone-tok-new")
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
            let full_queries = calls
                .lock()
                .unwrap()
                .iter()
                .filter(|&&call| call == "full")
                .count();
            assert!(
                full_queries
                    <= usize::try_from(crate::icloud::photos::MAX_EMPTY_PAGE_PROBES).unwrap(),
                "at most one bounded empty current inventory"
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

const METADATA_CHILD_BYTES: &[u8] =
    b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";

async fn metadata_child_page(server: &wiremock::MockServer) -> serde_json::Value {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path("/metadata.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(METADATA_CHILD_BYTES.to_vec()))
        .mount(server)
        .await;
    full_album_page_with_download(
        "PrimarySync",
        "master-PrimarySync",
        "metadata-baseline",
        &format!("{}/metadata.jpg", server.uri()),
        METADATA_CHILD_BYTES.len() as u64,
        &base64::engine::general_purpose::STANDARD.encode(Sha256::digest(METADATA_CHILD_BYTES)),
    )
}

/// Runs one full-enumeration cycle with XMP sidecars enabled over a
/// downloaded asset the provider now reports as a favourite.
async fn admit_metadata_child(
    inner: &Arc<state::SqliteStateDb>,
    directory: &std::path::Path,
    server: &wiremock::MockServer,
) -> (serde_json::Value, std::path::PathBuf) {
    let page = metadata_child_page(server).await;
    let library = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_owned_metadata_album(inner, "PrimarySync", page.clone()),
    );
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let build = make_run_cycle_download_config_builder(directory, db.clone());
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let result = Box::pin(run_cycle(
        &[&library],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        &build,
        download::DownloadControls::download_hidden(),
        &session,
        &CancellationToken::new(),
    ))
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.downloaded, 1);
    let rows = inner.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_ref(), "asset-master-PrimarySync");
    let path = rows[0].local_path.clone().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), METADATA_CHILD_BYTES.to_vec());
    (page, path)
}

/// Name an owned test album so two passes resolve distinct `{album}` paths.
#[cfg(feature = "xmp")]
fn make_named_metadata_album(
    db: &Arc<state::SqliteStateDb>,
    zone: &str,
    name: &str,
    page: serde_json::Value,
) -> crate::icloud::photos::PhotoAlbum {
    let album = make_owned_metadata_album(db, zone, page);
    let mut album = album.with_selector_components(vec![name.to_owned()]);
    album.name = Arc::from(name);
    album
}

/// Admits one asset under two equally current named albums so the later
/// race runs over two independently selected destinations.
#[cfg(feature = "xmp")]
async fn admit_two_album_metadata_children(
    inner: &Arc<state::SqliteStateDb>,
    directory: &std::path::Path,
    server: &wiremock::MockServer,
) -> (serde_json::Value, std::path::PathBuf) {
    let page = metadata_child_page(server).await;
    let pass = |name: &str| crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Album,
        album: make_named_metadata_album(inner, "PrimarySync", name, page.clone()),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    };
    let library = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![pass("Album A"), pass("Album B")],
    );
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let build = make_run_cycle_download_config_builder_with_options(
        directory,
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            xmp_sidecar: true,
            per_pass_paths: true,
            concurrent_downloads: Some(2),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let result = Box::pin(run_cycle(
        &[&library],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        &build,
        download::DownloadControls::download_hidden(),
        &session,
        &CancellationToken::new(),
    ))
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.downloaded, 2);
    let rows = inner.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id.as_ref(), "asset-master-PrimarySync");
    let path = rows[0].local_path.clone().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), METADATA_CHILD_BYTES.to_vec());
    (page, path)
}

#[cfg(feature = "xmp")]
async fn run_full_enumeration_metadata_cycle(
    inner: &Arc<state::SqliteStateDb>,
    download_dir: &std::path::Path,
    mut page: serde_json::Value,
    per_pass_paths: bool,
    controls: download::DownloadControls,
) -> crate::sync_cycle::CycleResult {
    let config = make_run_cycle_config();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    inner
        .acquire_lock("force full metadata cycle")
        .unwrap()
        .execute(
            "DELETE FROM metadata WHERE key=?1",
            ["sync_token:PrimarySync"],
        )
        .unwrap();
    page["syncToken"] = serde_json::json!("metadata-edited");
    page["records"][1]["fields"]["isFavorite"] = serde_json::json!({"value":1,"type":"INT64"});
    page["records"][1]["recordChangeTag"] = serde_json::json!("favourite-edit");
    let album = make_owned_metadata_album(inner, "PrimarySync", page);
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir,
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
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
        &build_download_config,
        controls,
        &shared_session,
        &CancellationToken::new(),
    ))
    .await
    .expect("run cycle")
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn current_metadata_selection_ignores_deselected_and_foreign_markers() {
    let server = crate::start_wiremock_or_skip!();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state.db");
    let inner = open_owned_cycle_db(&database).await;
    let (page, selected) = admit_metadata_child(&inner, directory.path(), &server).await;
    let mut historical = Vec::new();
    for library in ["PrimarySync", "ForeignLibrary"] {
        let path = directory.path().join(format!("unselected-{library}.jpg"));
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
        std::fs::write(&path, bytes).unwrap();
        std::fs::create_dir(format!("{}.xmp", path.display())).unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("unselected")
            .library(library)
            .filename("unselected.jpg")
            .metadata(state::AssetMetadata {
                rating: Some(5),
                ..Default::default()
            })
            .build();
        inner.upsert_seen(&record).await.unwrap();
        inner
            .mark_downloaded(
                library,
                "unselected",
                "original",
                &path,
                "historical-checksum",
                None,
            )
            .await
            .unwrap();
        inner
            .record_metadata_write_failure(library, "unselected", "original")
            .await
            .unwrap();
        historical.push((
            path.clone(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        ));
    }
    let markers = inner.get_metadata_retry_markers().await.unwrap();
    drop(inner);
    let mut prior_sidecar = None;
    for _ in 0..3 {
        let inner = open_owned_cycle_db(&database).await;
        let result = run_full_enumeration_metadata_cycle(
            &inner,
            directory.path(),
            page.clone(),
            false,
            download::DownloadControls::download_hidden(),
        )
        .await;
        assert_eq!(result.failed_count, 0);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("metadata-edited")
        );
        assert_eq!(inner.get_metadata_retry_markers().await.unwrap(), markers);
        for (path, modified) in &historical {
            assert_eq!(
                std::fs::read(path).unwrap(),
                include_bytes!("../../../../tests/data/media/pattern.jpg")
            );
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                *modified
            );
            assert!(std::path::Path::new(&format!("{}.xmp", path.display())).is_dir());
        }
        let sidecar = std::path::PathBuf::from(format!("{}.xmp", selected.display()));
        let current = (
            std::fs::read(&sidecar).unwrap(),
            std::fs::metadata(sidecar).unwrap().modified().unwrap(),
        );
        if let Some(prior) = prior_sidecar {
            assert_eq!(current, prior);
        }
        prior_sidecar = Some(current);
    }
}

/// #707 review: both enumeration branches defer their queue, so a full
/// enumeration drains once. A second drain in the same cycle would retry a
/// failed rewrite immediately and report it twice.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_full_enumeration_reports_a_failed_rewrite_once() {
    for per_pass_paths in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let inner = open_owned_cycle_db(&download_dir.path().join("state.db")).await;
        let (page, media_path) = admit_metadata_child(&inner, download_dir.path(), &server).await;
        let before = std::fs::read(&media_path).unwrap();
        // A directory where the sidecar belongs makes the rewrite fail.
        std::fs::create_dir(format!("{}.xmp", media_path.display()))
            .expect("block the sidecar path");

        let result = run_full_enumeration_metadata_cycle(
            &inner,
            download_dir.path(),
            page,
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
        assert_eq!(std::fs::read(media_path).unwrap(), before);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

/// #707 review: two named albums can retain independent physical copies of the
/// same asset. They are enumerated concurrently, so both copies must share the
/// single metadata drain, each must receive the durable row snapshot, and no
/// marker may survive. A legacy path from an older pass layout is not a current
/// writer; the fixture establishes both album copies under this authority first.
/// The passes carry different change tags but one provider snapshot because
/// current selection defers conflicting metadata snapshots for one asset.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_two_passes_over_one_asset_leave_the_sidecar_matching_the_row() {
    let server = crate::start_wiremock_or_skip!();
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let inner = open_owned_cycle_db(&download_dir.path().join("state.db")).await;
    let (mut page, media_path) =
        admit_two_album_metadata_children(&inner, download_dir.path(), &server).await;
    let before = std::fs::read(&media_path).unwrap();
    inner
        .acquire_lock("force two pass metadata enumeration")
        .unwrap()
        .execute(
            "DELETE FROM metadata WHERE key=?1",
            ["sync_token:PrimarySync"],
        )
        .unwrap();
    page["syncToken"] = serde_json::json!("metadata-race");
    page["records"][1]["recordChangeTag"] = serde_json::json!("favourite");
    page["records"][1]["fields"]["isFavorite"] = serde_json::json!({"value":1,"type":"INT64"});
    let mut second_pass = page.clone();
    second_pass["records"][1]["recordChangeTag"] = serde_json::json!("second-pass");
    inner.acquire_lock("observe native metadata finalization").unwrap().execute_batch(
        "CREATE TABLE metadata_retirements(path TEXT NOT NULL);
         CREATE TRIGGER observe_metadata_retirement AFTER UPDATE OF metadata_write_failed_at ON asset_metadata_paths
         WHEN OLD.metadata_write_failed_at IS NOT NULL AND NEW.metadata_write_failed_at IS NULL
         BEGIN INSERT INTO metadata_retirements VALUES(NEW.local_path); END;"
    ).unwrap();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let pass = |name: &str, page: serde_json::Value| crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Album,
        album: make_named_metadata_album(&inner, "PrimarySync", name, page),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    };
    let lib_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![pass("Album A", page), pass("Album B", second_pass)],
    );
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            xmp_sidecar: true,
            per_pass_paths: true,
            concurrent_downloads: Some(2),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
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
    .expect("run cycle");
    assert_eq!(result.failed_count, 0);
    assert_eq!(
        result.stats.downloaded, 0,
        "the preceding cycle already established both album copies"
    );
    assert!(!result.stats.sync_token_blocked);

    let reopened = state::SqliteStateDb::open(&download_dir.path().join("state.db"))
        .await
        .unwrap();
    let stored = reopened
        .get_downloaded_page(0, 10)
        .await
        .expect("read downloaded rows");
    let row = stored.first().expect("the asset stays downloaded");
    let finalized_paths: Vec<String> = reopened
        .acquire_lock("read finalized metadata destinations")
        .unwrap()
        .prepare("SELECT local_path FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-master-PrimarySync' AND version_size='original' ORDER BY local_path")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        finalized_paths.len(),
        2,
        "both album copies remain independent"
    );
    assert!(
        finalized_paths
            .iter()
            .any(|path| std::path::Path::new(path) == media_path)
    );
    for path in &finalized_paths {
        let sidecar = std::fs::read_to_string(format!("{path}.xmp"))
            .expect("the drain writes every finalized copy's sidecar");
        let retirements: i64 = reopened
            .acquire_lock("read per-path metadata completions")
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM metadata_retirements WHERE path=?1",
                [path],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retirements, 1,
            "one writer retires each physical path: {path}"
        );
        assert_eq!(
            sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
            row.metadata.rating == Some(5),
            "each sidecar must hold the durable snapshot: {sidecar}"
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
    assert!(
        reopened
            .get_metadata_retry_markers()
            .await
            .expect("read markers")
            .is_empty(),
        "no marker may survive a completed drain"
    );
    assert_eq!(std::fs::read(media_path).unwrap(), before);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

/// #707 review: a forwarded download writes the snapshot it was planned
/// from. If a concurrent pass has since refreshed the row and raised a
/// marker, the finaliser must not retire it, or the row keeps the new
/// metadata while the file keeps the old and nothing is left to repair it.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_download_does_not_retire_a_marker_raised_for_newer_metadata() {
    let server = crate::start_wiremock_or_skip!();
    let config = make_run_cycle_config();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let inner = open_owned_cycle_db(&download_dir.path().join("state.db")).await;
    let (mut page, media_path) = admit_metadata_child(&inner, download_dir.path(), &server).await;
    let before = std::fs::read(&media_path).unwrap();

    let frozen_hash = inner.get_downloaded_page(0, 10).await.unwrap()[0]
        .metadata
        .metadata_hash
        .clone();
    // The row stays downloaded but its file is gone, so the media task is
    // forwarded rather than skipped as already on disk.
    let retained = download_dir.path().join("retained-original.jpg");
    std::fs::rename(&media_path, &retained).expect("retain the missing destination's bytes");

    let mut newer = state::AssetMetadata {
        is_favorite: true,
        rating: Some(5),
        ..state::AssetMetadata::default()
    };
    newer.refresh_hash();
    {
        let conn = inner
            .acquire_lock("inject concurrent metadata edit at native publication")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE metadata_race(hash TEXT NOT NULL, fired INTEGER NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata_race VALUES(?1,0)",
            [newer.metadata_hash.as_deref().unwrap()],
        )
        .unwrap();
        conn.execute_batch("CREATE TRIGGER refresh_during_publication AFTER UPDATE OF verified_media ON provider_active_destinations
        WHEN NEW.library='PrimarySync' AND NEW.asset_id='asset-master-PrimarySync' AND NEW.version_size='original' AND OLD.verified_media=0 AND NEW.verified_media=1 AND (SELECT fired FROM metadata_race)=0
        BEGIN
          UPDATE metadata_race SET fired=1;
          UPDATE assets SET is_favorite=1,rating=5,metadata_hash=(SELECT hash FROM metadata_race),metadata_write_failed_at=1
            WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size;
          UPDATE asset_metadata_paths SET metadata_write_failed_at=1
            WHERE library=NEW.library AND id=NEW.asset_id AND version_size=NEW.version_size;
        END;").unwrap();
    }
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    page["syncToken"] = serde_json::json!("metadata-redownload");
    let album = make_owned_metadata_album(&inner, "PrimarySync", page);
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
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    let reopened = state::SqliteStateDb::open(&download_dir.path().join("state.db"))
        .await
        .unwrap();
    let fired: i64 = reopened
        .acquire_lock("assert native race ran")
        .unwrap()
        .query_row("SELECT fired FROM metadata_race", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fired, 1);
    assert_eq!(std::fs::read(&retained).unwrap(), before);
    assert_eq!(std::fs::read(&media_path).unwrap(), before);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    let row = reopened
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
        !sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
        "the newer row must not replace the frozen publication intent: {sidecar}"
    );
    assert_eq!(row.metadata.metadata_hash, newer.metadata_hash);
    let retained_marker: (Option<i64>, Option<i64>) = reopened
        .acquire_lock("read the newer durable metadata marker")
        .unwrap()
        .query_row(
            "SELECT a.metadata_write_failed_at,p.metadata_write_failed_at
             FROM assets a JOIN asset_metadata_paths p USING(library,id,version_size)
             WHERE a.library='PrimarySync' AND a.id='asset-master-PrimarySync'
               AND a.version_size='original' AND p.local_path=?1",
            [published.to_str().unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained_marker, (Some(1), Some(1)));
    let native_hashes: Vec<Option<String>> = reopened
        .acquire_lock("read frozen native metadata intent")
        .unwrap()
        .prepare(
            "SELECT metadata_hash FROM provider_active_destinations
             WHERE library='PrimarySync' AND asset_id='asset-master-PrimarySync'
               AND version_size='original' AND admitted=1 AND verified_media=1",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!native_hashes.is_empty());
    assert!(native_hashes.iter().all(|hash| hash == &frozen_hash));
    assert_ne!(frozen_hash, newer.metadata_hash);
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
        let server = crate::start_wiremock_or_skip!();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let inner = open_owned_cycle_db(&download_dir.path().join("state.db")).await;
        let (page, media_path) = admit_metadata_child(&inner, download_dir.path(), &server).await;
        let before = std::fs::read(&media_path).unwrap();
        inner
            .record_metadata_write_failure("PrimarySync", "asset-master-PrimarySync", "original")
            .await
            .expect("queue a rewrite");

        let markers_before = inner.get_metadata_retry_markers().await.unwrap();
        assert_eq!(markers_before.len(), 1);
        assert_eq!(
            inner
                .get_pending_metadata_rewrites_page(None, 0, 10)
                .await
                .unwrap()
                .len(),
            1
        );

        run_full_enumeration_metadata_cycle(&inner, download_dir.path(), page, false, controls)
            .await;

        assert!(
            !std::path::Path::new(&format!("{}.xmp", media_path.display())).exists(),
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
        assert_eq!(
            inner.get_metadata_retry_markers().await.unwrap(),
            markers_before
        );
        assert_eq!(std::fs::read(media_path).unwrap(), before);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

/// #707: the full-enumeration path shares the streaming producer with the
/// single-pass path, so a failed provider-metadata refresh must equally
/// stop the zone checkpoint from being stored.
#[tokio::test]
async fn run_cycle_full_enumeration_metadata_refresh_failure_preserves_zone_checkpoint() {
    Box::pin(exercise_selected_metadata_update(
        SelectedMetadataFault::StateWrite,
        1,
        SelectedMetadataRoute::Full,
    ))
    .await;
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
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let lookup = url.contains("/records/lookup?");
            let changes = url.contains("/changes/zone?");
            let query = url.contains("/records/query?");
            if lookup {
                self.lookups.fetch_add(1, Ordering::SeqCst);
            }
            if query {
                self.queries.fetch_add(1, Ordering::SeqCst);
            }
            if matches!(self.stage, Stage::Full | Stage::FullPerPass)
                && changes
                && request["zones"][0]["syncToken"] != "zone-after"
            {
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
                return Ok(serde_json::json!({"records": records}));
            }
            if changes {
                let records = if request["zones"][0]["syncToken"] == "zone-after" {
                    Vec::new()
                } else {
                    match self.stage {
                        Stage::AssetPair => vec![self.records[1].clone()],
                        Stage::AssetIdentity => {
                            let mut child = self.records[1].clone();
                            child["fields"].as_object_mut().unwrap().remove("masterRef");
                            vec![child]
                        }
                        Stage::LegacyCapture => self.records.clone(),
                        _ => Vec::new(),
                    }
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
            {
                let config = make_run_cycle_config();
                let dir = tempfile::tempdir().unwrap();
                let inner = open_owned_cycle_db(&dir.path().join("state.db")).await;
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
                let album = make_owned_cycle_album(
                    &inner,
                    "PrimarySync",
                    "private",
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
                        ..RunCycleDownloadConfigOptions::default()
                    },
                );
                let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
                let mut capture_lookups = 0;
                for cycle in 0..3 {
                    if cycle > 0 {
                        failing.store(false, Ordering::SeqCst);
                    }
                    let capture = if capture_pending {
                        let lookups_before = lookups.load(Ordering::SeqCst);
                        let (repair, failures) =
                            repair_historical_capture(&inner, "PrimarySync", &library.plan.passes)
                                .await;
                        capture_lookups += lookups.load(Ordering::SeqCst) - lookups_before;
                        if cycle == 0 {
                            assert!(
                                failures > 0,
                                "explicit maintenance must execute the session fault"
                            );
                            assert!(repair.sync_token_blocked);
                            assert_eq!(
                                inner
                                    .get_metadata("sync_token:PrimarySync")
                                    .await
                                    .unwrap()
                                    .as_deref(),
                                Some("zone-before")
                            );
                            assert_eq!(snapshot_files(&media_dir), before);
                            continue;
                        }
                        Some(repair)
                    } else {
                        None
                    };
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
                    assert_eq!(
                        result.session_expired,
                        cycle == 0,
                        "{stage:?} HTTP {status} cycle={cycle}"
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
                    if matches!(
                        stage,
                        Stage::Incremental | Stage::AssetPair | Stage::AssetIdentity
                    ) {
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
                        const SCOPE: &str = "Scope: recorded local state only; this does not establish iCloud completeness or verify local media checksums.";
                        if cycle == 0 {
                            assert_eq!(
                                status,
                                if matches!(stage, Stage::AssetPair | Stage::AssetIdentity) {
                                    format!(
                                        "Backup record: unresolved asset identity in 1 provider zone; checkpoint replay is required; last sync was interrupted; 1 enumeration error occurred in the last sync\n  {SCOPE}"
                                    )
                                } else {
                                    format!(
                                        "Backup record: last sync was interrupted; 1 enumeration error occurred in the last sync\n  {SCOPE}"
                                    )
                                }
                            );
                        } else {
                            assert_eq!(
                                status,
                                format!(
                                    "Backup record: last sync completed and no pending or failed assets are recorded\n  {SCOPE}"
                                )
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
                                capture.as_ref().unwrap().metadata_capture_refreshed,
                                usize::from(cycle == 1)
                            );
                            assert_eq!(capture.as_ref().unwrap().metadata_capture_remaining, 0);
                            assert!(
                                inner.get_downloaded_page(0, 1).await.unwrap()[0]
                                    .metadata
                                    .is_favorite
                            );
                        }
                    }
                }
                if stage == Stage::Capture {
                    assert_eq!(capture_lookups, 2);
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
    {
        for replay in [false, true] {
            for changing_snapshot in [false, true] {
                let count = 101;
                let directory = tempfile::tempdir().unwrap();
                let media = directory.path().join("media");
                let database = directory.path().join("state.db");
                {
                    let inner = open_owned_cycle_db(&database).await;
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
                let config = make_run_cycle_config();
                let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
                let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let snapshot = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut library = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    make_owned_cycle_album(
                        &open_owned_cycle_db(&database).await,
                        "PrimarySync",
                        "private",
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
                    let inner = open_owned_cycle_db(&database).await;
                    bind_metadata_capture(
                        &inner,
                        "PrimarySync",
                        "private",
                        &mut library.plan.passes[0].album,
                    );
                    let conn = rusqlite::Connection::open(&database).unwrap();
                    conn.execute_batch("DROP TRIGGER IF EXISTS fail_cached_sparse_retirement")
                        .unwrap();
                    if cycle == 1 {
                        conn.execute_batch("CREATE TRIGGER fail_cached_sparse_retirement BEFORE DELETE ON unresolved_sparse_identities
                            WHEN OLD.library='PrimarySync' BEGIN SELECT RAISE(ABORT,'injected cached deletion state failure'); END;").unwrap();
                    }
                    drop(conn);
                    let db: Arc<dyn download::DownloadStore> = inner.clone();
                    let builder = make_run_cycle_download_config_builder_with_options(
                        &media,
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
                        "replay={replay} changing_snapshot={changing_snapshot} cycle={cycle}"
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
    {
        for change in [
            ValidationChange::Restored,
            ValidationChange::Incomplete,
            ValidationChange::Cancelled,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let media = directory.path().join("media");
            let database = directory.path().join("state.db");
            {
                let inner = open_owned_cycle_db(&database).await;
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
            let config = make_run_cycle_config();
            let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let validations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let cycles = if change == ValidationChange::Restored {
                2
            } else {
                4
            };
            for cycle in 0..cycles {
                let inner = open_owned_cycle_db(&database).await;
                let db = inner.clone() as Arc<dyn download::DownloadStore>;
                let shutdown = CancellationToken::new();
                let library = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    make_owned_cycle_album(
                        &inner,
                        "PrimarySync",
                        "private",
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
                    RunCycleDownloadConfigOptions::default(),
                );
                let result = run_cycle(
                    &[&library],
                    &config,
                    Some(db.as_ref()),
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
                    "{change:?} cycle {cycle}"
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
        cancel_after_observation: Option<CancellationToken>,
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
                if url.contains("/records/query?")
                    && page["records"].as_array().is_some_and(Vec::is_empty)
                    && let Some(cancel) = &self.cancel_after_observation
                {
                    cancel.cancel();
                }
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
            assert!(
                matches!(
                    names.as_slice(),
                    ["asset-recovered"] | ["recovered"] | ["asset-recovered", "recovered"]
                ),
                "only the current child and its exact source may be confirmed: {names:?}"
            );
            self.lookups
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(cancel) = &self.cancel_after_observation {
                cancel.cancel();
            }
            let records: Vec<_> = self
                .records
                .as_array()
                .unwrap()
                .iter()
                .filter(|record| names.contains(&record["recordName"].as_str().unwrap()))
                .collect();
            Ok(serde_json::json!({"records":records}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    {
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
                let db = open_owned_cycle_db(&database).await;
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
            let config = make_run_cycle_config();
            let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
            let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut completed_requests = 0;
            for cycle in 0..3 {
                let inner = open_owned_cycle_db(&database).await;
                let cancel = CancellationToken::new();
                let conn = rusqlite::Connection::open(&database).unwrap();
                conn.execute_batch("DROP TRIGGER IF EXISTS fail_sparse_selected_upsert")
                    .unwrap();
                if cycle == 0 && !interrupted {
                    conn.execute_batch(
                        "CREATE TRIGGER fail_sparse_selected_upsert BEFORE INSERT ON assets
                        WHEN NEW.library='PrimarySync' AND NEW.id='asset-recovered'
                        BEGIN SELECT RAISE(ABORT,'injected sparse recovery state failure'); END;",
                    )
                    .unwrap();
                }
                drop(conn);
                let db: Arc<dyn download::DownloadStore> = inner.clone();
                let primary = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    make_owned_cycle_album(
                        &inner,
                        "PrimarySync",
                        "private",
                        Box::new(RecoveredSession {
                            records: records.clone(),
                            lookups: lookups.clone(),
                            cancel_after_observation: (cycle == 0 && interrupted)
                                .then(|| cancel.clone()),
                        }),
                    ),
                );
                let builder = make_run_cycle_download_config_builder_with_options(
                    &media,
                    db.clone(),
                    RunCycleDownloadConfigOptions::default(),
                );
                let lookups_before = lookups.load(std::sync::atomic::Ordering::Relaxed);
                let sparse_hydration = usize::from(
                    !inner
                        .sparse_identities("PrimarySync")
                        .await
                        .unwrap()
                        .is_empty(),
                );
                let retained_confirmation = usize::from(inner.acquire_lock("bound retained confirmation phases").unwrap()
                    .query_row("SELECT EXISTS(SELECT 1 FROM provider_active_generations WHERE checkpoint_ready=0)", [], |row| row.get::<_, bool>(0)).unwrap());
                let result = run_cycle(
                    &[&primary],
                    &config,
                    Some(db.as_ref()),
                    &builder,
                    download::DownloadControls::download_hidden(),
                    &shared_session,
                    &cancel,
                )
                .await
                .unwrap();
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep these bytes");
                assert!(
                    lookups.load(std::sync::atomic::Ordering::Relaxed) - lookups_before
                        <= 2 * (1
                            + sparse_hydration
                            + retained_confirmation
                            + result.stats.same_cycle_recovery_attempts),
                    "each sparse hydration, current admission, and retained confirmation gets at most one child/source pair: cycle={cycle}, interrupted={interrupted}, lookups={}, stats={:?}",
                    lookups.load(std::sync::atomic::Ordering::Relaxed) - lookups_before,
                    result.stats
                );
                assert_eq!(
                    inner
                        .get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(if cycle == 0 { "before" } else { "after" }),
                    "interrupted={interrupted} cycle={cycle}"
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
            assert!(
                lookups.load(std::sync::atomic::Ordering::Relaxed) >= 2,
                "recovery confirms both selected identities"
            );
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
                let db = open_owned_cycle_db(&db_path).await;
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
                if with_sibling {
                    // This smart-folder selection cannot establish complete
                    // family membership, especially for a visible sibling.
                    // Seed the competing historical identity that makes the
                    // legacy receipt ambiguous independently of current scope.
                    db.upsert_asset_master_mapping(ZONE, "valid-sibling", MASTER)
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
                    zone_id: Arc::new(
                        json!({"zoneName": ZONE, "ownerRecordName": "_defaultOwner"}),
                    ),
                    retry_config: retry::RetryConfig::default(),
                    container_id: None,
                    cross_zone_sources: Vec::new(),
                },
                Box::new(crate::sync_loop::test_support::MutableCaptureSession {
                    token: "zone-tok-new",
                    zone: ZONE,
                    cancel_after_lookup: None,
                    records: Arc::clone(&provider_records),
                    repair_requests: Arc::clone(&repair_requests),
                }),
            );
            let mut lib_state = make_run_cycle_library_state_with_passes(
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
                        "CREATE TRIGGER fail_capture_refresh BEFORE UPDATE OF created_at ON assets WHEN OLD.id='master-hidden' BEGIN SELECT RAISE(ABORT, 'injected capture refresh failure'); END;"
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
                let inner = open_owned_cycle_db(&db_path).await;
                bind_metadata_capture(&inner, ZONE, "shared", &mut lib_state.plan.passes[0].album);
                let before_lookups = repair_requests.load(Ordering::SeqCst);
                let (capture, _) =
                    repair_historical_capture(&inner, ZONE, &lib_state.plan.passes).await;
                let capture_lookups = repair_requests.load(Ordering::SeqCst) - before_lookups;
                assert_eq!(capture.sync_token_blocked, !recovered);
                if cycle == 2 && !with_sibling && !valid_epoch {
                    assert_eq!(
                        capture.metadata_capture_failures, 1,
                        "native capture SQL fault executed"
                    );
                }
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
                    &build_config,
                    download::DownloadControls::download_hidden(),
                    &session,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(
                    result.failed_count > 0,
                    !valid_epoch && cycle < 2,
                    "invalid={invalid:?}, sibling={with_sibling}, cycle={cycle}"
                );
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(
                    capture.metadata_capture_refreshed,
                    usize::from(!with_sibling && if valid_epoch { cycle == 0 } else { cycle == 3 })
                );
                assert_eq!(capture.metadata_capture_remaining, u64::from(!recovered));
                drop(build_config);
                drop(db);
                drop(inner);
                let inner = state::SqliteStateDb::open(&db_path).await.unwrap();
                if with_sibling {
                    assert_eq!(
                        inner
                            .get_master_record_name_for_asset(ZONE, "valid-sibling")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some(MASTER),
                        "competing historical identity survives every reopen"
                    );
                }
                assert_eq!(
                    inner.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
                    Some(if valid_epoch || cycle >= 2 {
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
                assert!(
                    inner
                        .get_pending_metadata_rewrites(10)
                        .await
                        .unwrap()
                        .is_empty()
                );
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
                    "SELECT count(*) FROM metadata_capture_retries WHERE library=?1 AND asset_id=?2 AND attempts=1 AND next_retry_at>last_attempt_at",
                    [ZONE, MASTER], |row| row.get(0),
                ).unwrap();
                assert_eq!(retries, i64::from(with_sibling));
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
                    if completed_requests.is_some() {
                        assert_eq!(
                            capture_lookups, 0,
                            "completed explicit repair has no historical followup lookup"
                        );
                    }
                    completed_requests = Some(capture_lookups);
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
    PreserveHistoricalClaim,
    #[cfg_attr(
        not(unix),
        expect(
            dead_code,
            reason = "The retained-inode replacement fixture requires Unix namespace semantics"
        )
    )]
    PreserveHistoricalReplacement,
    PreserveConfigChange,
    PreserveIncompleteInventory,
    PreserveInventoryRecovery,
    PreserveFreshRetry,
    PreserveSharedOriginal,
    PreserveCheckpointFailure,
    PreserveCompanion,
    PreservePaginatedHidden,
    PreserveHiddenUnselected,
    PreserveUnknownSource,
    PreserveBridgeDebt,
    PreserveMissingDelta,
    PreserveBlankDelta,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyHardlinkFault {
    Quiet,
    Disable,
    DisableCurrentOnly,
    OriginalBytes,
    OriginalSidecarMissing,
    OriginalTemp,
    #[cfg(target_os = "linux")]
    OriginalJournal,
    CurrentMissing,
}

async fn exercise_legacy_child_cycles(fault: AmbiguousChildFault, child_counts: &[usize]) {
    Box::pin(exercise_legacy_child_cycles_with_hardlinks(
        fault,
        child_counts,
        None,
    ))
    .await;
}

async fn exercise_legacy_child_cycles_with_hardlinks(
    fault: AmbiguousChildFault,
    child_counts: &[usize],
    hardlink_fault: Option<LegacyHardlinkFault>,
) {
    Box::pin(exercise_legacy_child_cycles_with_selection(
        fault,
        child_counts,
        hardlink_fault,
        false,
    ))
    .await;
}

async fn exercise_legacy_child_cycles_with_selection(
    fault: AmbiguousChildFault,
    child_counts: &[usize],
    hardlink_fault: Option<LegacyHardlinkFault>,
    smart_only: bool,
) {
    Box::pin(exercise_legacy_child_cycles_with_provenance(
        fault,
        child_counts,
        hardlink_fault,
        smart_only,
        None,
    ))
    .await;
}

async fn exercise_legacy_child_cycles_with_provenance(
    fault: AmbiguousChildFault,
    child_counts: &[usize],
    hardlink_fault: Option<LegacyHardlinkFault>,
    smart_only: bool,
    signature_bytes: Option<usize>,
) {
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
        inventory_requests: Arc<std::sync::atomic::AtomicUsize>,
        lookup_requests: Arc<std::sync::atomic::AtomicUsize>,
        quiet_token: Option<&'static str>,
        cancel_after_page: Option<CancellationToken>,
        replace_after_page: Vec<std::path::PathBuf>,
        injected_faults: Arc<std::sync::atomic::AtomicUsize>,
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
            if url.contains("/changes/zone?") && request["zones"][0]["syncToken"].is_null() {
                self.inventory_requests
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
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
                let mut response = serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":self.quiet_token,"moreComing":false,"records":[]}]});
                if self.quiet_token.is_none() {
                    response["zones"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("syncToken");
                }
                return Ok(response);
            }
            if url.contains("/changes/zone?") {
                let records: Vec<_> = self
                    .records
                    .iter()
                    .filter(|record| {
                        !self.omit_hidden
                            || record["fields"]["isHidden"]["value"] != serde_json::json!(1)
                    })
                    .collect();
                let mut response = serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"after","moreComing":false,"records":records}]});
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
                    self.records
                        .iter()
                        .filter(|record| {
                            record["recordType"] == "CPLAsset"
                                && (!self.omit_hidden || record["fields"]["isHidden"]["value"] != 1)
                        })
                        .count() as u64
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
                if offset > 0 {
                    if let Some(cancel) = &self.cancel_after_page {
                        cancel.cancel();
                        self.injected_faults
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    for path in &self.replace_after_page {
                        let bytes = std::fs::read(path)?;
                        let replacement = path.with_extension("replacement");
                        std::fs::write(&replacement, bytes)?;
                        std::fs::rename(replacement, path)?;
                        self.injected_faults
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
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
            self.lookup_requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let names = request["records"].as_array().unwrap();
            for name in names {
                assert!(
                    self.records.iter().any(|record| {
                        name["recordName"] == record["recordName"]
                            || name["recordName"]
                                == record["fields"]["masterRef"]["value"]["recordName"]
                    }),
                    "lookup must belong to an observed current child or its source"
                );
            }
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
            | AmbiguousChildFault::PreserveHistoricalClaim
            | AmbiguousChildFault::PreserveHistoricalReplacement
            | AmbiguousChildFault::PreserveConfigChange
            | AmbiguousChildFault::PreserveIncompleteInventory
            | AmbiguousChildFault::PreserveInventoryRecovery
            | AmbiguousChildFault::PreserveFreshRetry
            | AmbiguousChildFault::PreserveSharedOriginal
            | AmbiguousChildFault::PreserveCheckpointFailure
            | AmbiguousChildFault::PreserveCompanion
            | AmbiguousChildFault::PreservePaginatedHidden
            | AmbiguousChildFault::PreserveHiddenUnselected
            | AmbiguousChildFault::PreserveUnknownSource
            | AmbiguousChildFault::PreserveBridgeDebt
            | AmbiguousChildFault::PreserveMissingDelta
            | AmbiguousChildFault::PreserveBlankDelta
            | AmbiguousChildFault::PreserveCancel
            | AmbiguousChildFault::PreserveStateWrite
            | AmbiguousChildFault::PreservePathConflict
    );
    for &children in child_counts {
        for sidecars in [false, true] {
            let server = crate::start_wiremock_or_skip!();
            let bytes =
                b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
            let checksum = match signature_bytes {
                Some(length) => {
                    base64::engine::general_purpose::STANDARD.encode(vec![0x72; length])
                }
                None => base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)),
            };
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
                child["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                    serde_json::json!("_defaultOwner");
                if smart_only {
                    child["fields"]["isHidden"] = serde_json::json!({"value":1});
                }
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
            let original_mtime = std::fs::metadata(&legacy_path).unwrap().modified().unwrap();
            let original_sidecar_mtime =
                std::fs::metadata(&legacy_xmp).unwrap().modified().unwrap();
            let external_alias = dir.path().join("importer-alias.jpg");
            if matches!(fault, AmbiguousChildFault::PreserveSharedOriginal)
                || hardlink_fault
                    .is_some_and(|fault| fault != LegacyHardlinkFault::DisableCurrentOnly)
            {
                std::fs::hard_link(&legacy_path, &external_alias).unwrap();
                if hardlink_fault.is_some() {
                    std::fs::hard_link(
                        &legacy_xmp,
                        dir.path().join("importer-original-sidecar.xmp"),
                    )
                    .unwrap();
                }
            }
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
                let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
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
                if preserving {
                    // Current selection drift, not historical certificates,
                    // requires the initial selected inventory and delta bridge.
                    inner
                        .set_metadata(
                            crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
                            "previous-selection",
                        )
                        .await
                        .unwrap();
                }
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
                if !matches!(fault, AmbiguousChildFault::PreserveFreshRetry) {
                    assert!(
                        inner
                            .defer_metadata_capture_ambiguity(&candidate, 1)
                            .await
                            .unwrap()
                    );
                }
            }
            if matches!(
                fault,
                AmbiguousChildFault::PreserveHistoricalClaim
                    | AmbiguousChildFault::PreserveHistoricalReplacement
            ) {
                let db = state::SqliteStateDb::open(&database).await.unwrap();
                let date =
                    chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
                for (index, child) in names.iter().enumerate() {
                    let asset = crate::icloud::photos::PhotoAsset::from_records(
                        serde_json::from_value(records[0].clone()).unwrap(),
                        &serde_json::from_value(records[index + 1].clone()).unwrap(),
                    );
                    let current = destination.join(format!("independent-{index}.jpg"));
                    std::fs::write(&current, bytes).unwrap();
                    let hash = crate::download::file::compute_sha256(&current)
                        .await
                        .unwrap();
                    db.upsert_seen(
                        &crate::test_helpers::TestAssetRecord::new(child)
                            .filename("photo.jpg")
                            .created_at(date)
                            .added_at(date)
                            .checksum(&checksum)
                            .size(bytes.len() as u64)
                            .metadata(
                                (*asset.metadata_arc(state::VersionSizeKey::Original)).clone(),
                            )
                            .build(),
                    )
                    .await
                    .unwrap();
                    state::DownloadStateStore::mark_verified_download(
                        &db,
                        "PrimarySync",
                        child,
                        "original",
                        &current,
                        &hash,
                        Some(&hash),
                        false,
                        &Default::default(),
                    )
                    .await
                    .unwrap();
                    if sidecars {
                        std::fs::write(
                            current.with_extension("jpg.xmp"),
                            b"independent current sidecar",
                        )
                        .unwrap();
                    }
                }
                drop(db);
                let conn = rusqlite::Connection::open(&database).unwrap();
                for child in &names {
                    conn.execute(
                        "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum)
                         VALUES ('PrimarySync',?1,'original',?2,?3,'historical-local')",
                        rusqlite::params![child,legacy_path.to_str().unwrap(),checksum],
                    ).unwrap();
                }
            }
            let historical_child_receipts = rows(
                &database,
                "SELECT * FROM asset_metadata_paths WHERE id!='legacy-master' AND local_checksum='historical-local' ORDER BY id",
            );
            if matches!(fault, AmbiguousChildFault::PreserveCheckpointFailure) {
                rusqlite::Connection::open(&database).unwrap().execute_batch(
                    "CREATE TRIGGER fail_single_survivor_cursor BEFORE UPDATE ON metadata WHEN OLD.key='sync_token:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected checkpoint cursor failure'); END;"
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
            let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
            let mut completed_requests = 0;
            let mut completed_outputs =
                std::collections::HashMap::<std::path::PathBuf, Vec<u8>>::new();
            let mut retained_proofs = Vec::new();
            let mut retained_original = Vec::new();
            let mut retained_paths = Vec::new();
            let legacy_metadata_paths_sql =
                "SELECT * FROM asset_metadata_paths WHERE id='legacy-master' ORDER BY local_path";
            let legacy_metadata_paths = rows(&database, legacy_metadata_paths_sql);
            let inventory_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let lookup_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let relocating = matches!(
                fault,
                AmbiguousChildFault::PreserveConfigChange
                    | AmbiguousChildFault::PreserveInventoryRecovery
            );
            let fresh_source_recovery = matches!(
                fault,
                AmbiguousChildFault::StateWrite | AmbiguousChildFault::PreserveStateWrite
            );
            let mut retained_unverified_outputs = Vec::new();
            let mut missing_current_path = None;
            for cycle in 0..if relocating
                || fresh_source_recovery
                || matches!(fault, AmbiguousChildFault::PreserveHistoricalReplacement)
            {
                5
            } else {
                4
            } {
                let mut config = make_run_cycle_config();
                let allow_hardlinks = hardlink_fault.is_some()
                    && !(cycle >= 2
                        && matches!(
                            hardlink_fault,
                            Some(
                                LegacyHardlinkFault::Disable
                                    | LegacyHardlinkFault::DisableCurrentOnly
                            )
                        ));
                config.download.legacy_preservation_allow_hardlinks = allow_hardlinks;
                if relocating && cycle >= 2 {
                    config.download.folder_structure = "relocated".into();
                    config.download.folder_structure_albums = "relocated".into();
                    config.download.folder_structure_smart_folders = "relocated".into();
                }
                let mut missing_current = missing_current_path.clone();
                let mut current_aliases = std::collections::HashMap::new();
                if cycle == 2
                    && let Some(hardlink_fault) = hardlink_fault
                {
                    for (index, path) in completed_outputs.keys().enumerate() {
                        let alias = dir.path().join(format!("importer-current-{index}"));
                        std::fs::hard_link(path, &alias).unwrap();
                        current_aliases.insert(alias, std::fs::read(path).unwrap());
                    }
                    match hardlink_fault {
                        LegacyHardlinkFault::OriginalBytes => {
                            let mut changed = bytes.to_vec();
                            changed[15] ^= 1;
                            std::fs::write(&legacy_path, changed).unwrap();
                        }
                        LegacyHardlinkFault::OriginalSidecarMissing => {
                            std::fs::remove_file(&legacy_xmp).unwrap()
                        }
                        LegacyHardlinkFault::OriginalTemp => std::fs::write(
                            legacy_path.with_file_name("photo.jpg.part"),
                            b"pending legacy temporary",
                        )
                        .unwrap(),
                        #[cfg(target_os = "linux")]
                        LegacyHardlinkFault::OriginalJournal => {
                            let digest =
                                data_encoding::HEXLOWER.encode(&Sha256::digest(b"photo.jpg"));
                            std::fs::create_dir(
                                legacy_path.with_file_name(format!(".kei-replace-{digest}")),
                            )
                            .unwrap();
                        }
                        LegacyHardlinkFault::CurrentMissing => {
                            let current_receipts = state::SqliteStateDb::open(&database)
                                .await
                                .unwrap()
                                .get_downloaded_page(0, 20)
                                .await
                                .unwrap();
                            let current = current_receipts
                                .iter()
                                .find(|row| {
                                    row.id.as_ref() == names[0]
                                        && row.version_size == state::VersionSizeKey::Original
                                })
                                .expect("persisted current child media receipt")
                                .local_path
                                .as_ref()
                                .expect("current child media receipt path");
                            assert!(
                                completed_outputs.contains_key(current),
                                "missing-current fault must target a previously verified output"
                            );
                            std::fs::remove_file(current).unwrap();
                            assert!(!current.exists(), "current receipt path must be absent");
                            missing_current = Some(current.clone());
                            missing_current_path = missing_current.clone();
                            // Use production drift detection below to wake ordinary
                            // recovery. The HTTP refusal must retain actual retry work.
                            server.reset().await;
                            Mock::given(method("GET"))
                                .and(path("/child.jpg"))
                                .respond_with(ResponseTemplate::new(404))
                                .mount(&server)
                                .await;
                        }
                        _ => {}
                    }
                }
                let original_before = hardlink_fault.map(|_| {
                    (
                        std::fs::read(&legacy_path).unwrap(),
                        std::fs::read(&legacy_xmp).ok(),
                    )
                });
                let requests_before = server.received_requests().await.unwrap().len();
                if cycle == 1
                    && sidecars
                    && cfg!(feature = "xmp")
                    && matches!(
                        fault,
                        AmbiguousChildFault::PreserveHistoricalClaim
                            | AmbiguousChildFault::PreserveHistoricalReplacement
                    )
                {
                    // Preserve the unowned sidecars after their first-cycle
                    // veto, before permitting fresh selected metadata output.
                    for index in 0..children {
                        let path = destination.join(format!("independent-{index}.jpg.xmp"));
                        let saved = dir.path().join(format!("retained-current-{index}.xmp"));
                        let contents = std::fs::read(&path).unwrap();
                        std::fs::rename(path, &saved).unwrap();
                        retained_unverified_outputs.push((saved, contents));
                    }
                }
                let lookups_before = lookup_requests.load(std::sync::atomic::Ordering::SeqCst);
                let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
                if cycle == 2 && missing_current.is_some() {
                    let drift = crate::sync_loop::reconcile::run_bounded_local_drift_probe(
                        inner.as_ref(),
                        cycle,
                    )
                    .await;
                    assert!(
                        drift.marked_failed > 0,
                        "missing current files must wake normal recovery"
                    );
                }
                let cancel = CancellationToken::new();
                let conn = rusqlite::Connection::open(&database).unwrap();
                conn.execute_batch("DROP TRIGGER IF EXISTS fail_legacy_child_download")
                    .unwrap();
                if cycle == 0
                    && matches!(
                        fault,
                        AmbiguousChildFault::StateWrite | AmbiguousChildFault::PreserveStateWrite
                    )
                {
                    conn.execute_batch(
                        "CREATE TRIGGER fail_legacy_child_download BEFORE UPDATE OF status ON assets
                         WHEN NEW.status='downloaded' AND NEW.id!='legacy-master'
                         BEGIN SELECT RAISE(ABORT,'injected child download finalization failure'); END;",
                    )
                    .unwrap();
                }
                drop(conn);
                let db: Arc<dyn download::DownloadStore> = inner.clone();
                let injected_faults = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let provider = ChildSession {
                    // A selected child with an unavailable master is unresolved
                    // provider identity, not a successful unverified download.
                    records: if children == 0 {
                        Vec::new()
                    } else if matches!(fault, AmbiguousChildFault::PreserveUnknownSource) {
                        records[1..].to_vec()
                    } else {
                        records.clone()
                    },
                    complete: true,
                    bridge_debt: matches!(fault, AmbiguousChildFault::PreserveBridgeDebt),
                    paginated: matches!(fault, AmbiguousChildFault::PreservePaginatedHidden),
                    omit_hidden: matches!(fault, AmbiguousChildFault::PreserveHiddenUnselected),
                    incomplete_inventory: matches!(
                        fault,
                        AmbiguousChildFault::PreserveIncompleteInventory
                    ) || (cycle == 0
                        && matches!(fault, AmbiguousChildFault::PreserveInventoryRecovery)),
                    inventory_requests: Arc::clone(&inventory_requests),
                    lookup_requests: Arc::clone(&lookup_requests),
                    injected_faults: injected_faults.clone(),
                    cancel_after_page: (cycle == 0
                        && matches!(
                            fault,
                            AmbiguousChildFault::Cancel | AmbiguousChildFault::PreserveCancel
                        ))
                    .then(|| cancel.clone()),
                    replace_after_page: if cycle == 0
                        && matches!(fault, AmbiguousChildFault::PreserveHistoricalReplacement)
                    {
                        (0..children)
                            .map(|index| destination.join(format!("independent-{index}.jpg")))
                            .collect()
                    } else {
                        Vec::new()
                    },
                    quiet_token: if matches!(fault, AmbiguousChildFault::PreserveMissingDelta) {
                        None
                    } else if matches!(fault, AmbiguousChildFault::PreserveBlankDelta) {
                        Some("")
                    } else if smart_only {
                        Some("delta-after")
                    } else if hardlink_fault.is_some() && cycle >= 2 {
                        Some("later")
                    } else {
                        Some("after")
                    },
                };
                let mut primary = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    crate::sync_loop::test_support::make_owned_cycle_album(
                        &inner,
                        "PrimarySync",
                        "private",
                        Box::new(provider.clone()),
                    ),
                );
                if preserving {
                    primary.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                        Box::new(provider.clone()),
                        "PrimarySync",
                    );
                }
                if smart_only {
                    let (capture, _, _) = primary.plan.passes[0]
                        .album
                        .owned_selection_scope()
                        .unwrap()
                        .expect("owned fixture selection");
                    primary.plan.passes[0].kind = crate::commands::PassKind::SmartFolder;
                    primary.pass_scope.include_smart_folders = true;
                    primary.pass_scope.include_unfiled = false;
                    // Use the real Hidden query definition, with a fully synthetic session.
                    primary.plan.passes[0].album = crate::icloud::photos::PhotoAlbum::new(
                        crate::icloud::photos::PhotoAlbumConfig {
                            params: Arc::new(std::collections::HashMap::new()),
                            service_endpoint: Arc::from("https://example.com"),
                            name: Arc::from("Hidden"),
                            list_type: Arc::from("CPLAssetAndMasterHiddenByAssetDate"),
                            obj_type: Arc::from("CPLAssetHiddenByAssetDate"),
                            query_filter: None,
                            page_size: 100,
                            zone_id: Arc::new(
                                serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                            ),
                            retry_config: crate::retry::RetryConfig::default(),
                            container_id: None,
                            cross_zone_sources: Vec::new(),
                        },
                        Box::new(provider.clone()),
                    );
                    primary.plan.passes[0]
                        .album
                        .set_shadow_capture(capture, Arc::from("private"));
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
                    Arc::make_mut(&mut built).legacy_preservation_allow_hardlinks = allow_hardlinks;
                    if relocating && cycle >= 2 {
                        let current = Arc::make_mut(&mut built);
                        current.folder_structure = "relocated".into();
                        current.folder_structure_albums = Arc::from("relocated");
                        current.folder_structure_smart_folders = Arc::from("relocated");
                    }
                    built
                };
                let warning_path = dir.path().join(format!("cycle-{cycle}.log"));
                let subscriber = tracing_subscriber::fmt()
                    .with_ansi(false)
                    .without_time()
                    .with_max_level(tracing::Level::WARN)
                    .with_writer(std::sync::Mutex::new(
                        std::fs::File::create(&warning_path).unwrap(),
                    ))
                    .finish();
                use tracing::instrument::WithSubscriber as _;
                let result = run_cycle(
                    &[&primary],
                    &config,
                    Some(db.as_ref()),
                    &builder,
                    download::DownloadControls::download_hidden(),
                    &shared_session,
                    &cancel,
                )
                .with_subscriber(subscriber)
                .await;
                if cycle == 0
                    && matches!(
                        fault,
                        AmbiguousChildFault::Cancel
                            | AmbiguousChildFault::PreserveCancel
                            | AmbiguousChildFault::PreserveHistoricalReplacement
                    )
                {
                    assert!(
                        injected_faults.load(std::sync::atomic::Ordering::SeqCst) > 0,
                        "provider transition must exercise the configured fault"
                    );
                }
                if cycle == 0 && matches!(fault, AmbiguousChildFault::PreserveHistoricalReplacement)
                {
                    // Namespace churn carries no authority to adopt historical
                    // originals. The common oracle below checks both receipts.
                    for index in 0..children {
                        assert_eq!(
                            std::fs::read(destination.join(format!("independent-{index}.jpg")))
                                .unwrap(),
                            bytes
                        );
                    }
                    assert_eq!(
                        server.received_requests().await.unwrap().len(),
                        requests_before
                    );
                }
                if cycle >= 2
                    && hardlink_fault.is_some_and(|fault| fault != LegacyHardlinkFault::Quiet)
                {
                    let original_journal = match hardlink_fault {
                        #[cfg(target_os = "linux")]
                        Some(LegacyHardlinkFault::OriginalJournal) => true,
                        _ => false,
                    };
                    if original_journal {
                        assert!(
                            result.as_ref().unwrap_err().to_string().contains(
                                "Replacement recovery intersects protected legacy evidence"
                            )
                        );
                    }
                    let result = if original_journal {
                        None
                    } else {
                        Some(result.as_ref().unwrap())
                    };
                    if missing_current.is_some() {
                        let result = result.unwrap();
                        assert!(
                            result.failed_count > 0,
                            "failed current repair remains unfinished"
                        );
                        assert!(inner.get_summary().await.unwrap().failed > 0);
                    } else if let Some(result) = result {
                        assert_eq!(
                            result.failed_count, 0,
                            "historical change is not current work"
                        );
                        assert!(!result.stats.sync_token_blocked);
                    }
                    assert_eq!(
                        result.map_or(0, |result| result.stats.downloaded),
                        0,
                        "historical changes must not publish new outputs"
                    );
                    assert!(
                        inner
                            .legacy_preservations("PrimarySync")
                            .await
                            .unwrap()
                            .is_empty()
                    );
                    assert_eq!(
                        inner
                            .get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some(if missing_current.is_some() || original_journal {
                            "after"
                        } else {
                            "later"
                        })
                    );
                    assert_eq!(rows(&database, receipt_sql), receipt);
                    assert_eq!(rows(&database, retry_sql), retry);
                    assert_eq!(rows(&database, revision_sql), revision);
                    assert_eq!(rows(&database, mapping_sql), mappings);
                    assert_eq!(
                        rows(&database, legacy_metadata_paths_sql),
                        legacy_metadata_paths
                    );
                    assert_eq!(
                        rows(
                            &database,
                            "SELECT evidence_version,original_evidence,files,prepared_at FROM unattributed_legacy"
                        ),
                        retained_original
                    );
                    assert_eq!(
                        rows(
                            &database,
                            "SELECT * FROM unattributed_legacy_paths ORDER BY path_key"
                        ),
                        retained_paths
                    );
                    let proofs = rows(
                        &database,
                        "SELECT * FROM unattributed_legacy_proofs ORDER BY generation",
                    );
                    assert_eq!(
                        retained_proofs, proofs,
                        "historical proof bytes are unchanged"
                    );
                    let (original, sidecar) = original_before.unwrap();
                    assert_eq!(std::fs::read(&legacy_path).unwrap(), original);
                    assert_eq!(std::fs::read(&legacy_xmp).ok(), sidecar);
                    if let Some(path) = &missing_current {
                        assert!(
                            !path.exists(),
                            "failed repair must leave Current media absent"
                        );
                        assert!(
                            server.received_requests().await.unwrap().len() > requests_before,
                            "missing Current must exercise the rejected provider repair"
                        );
                        assert_eq!(
                            proofs, retained_proofs,
                            "missing evidence must not mint a proof"
                        );
                        assert!(result.unwrap().stats.sync_token_blocked);
                    } else {
                        assert_eq!(
                            server.received_requests().await.unwrap().len(),
                            requests_before,
                            "qualification fault must hold before a new transfer"
                        );
                    }
                    for (alias, expected) in &current_aliases {
                        assert_eq!(
                            &std::fs::read(alias).unwrap(),
                            expected,
                            "external Current alias bytes must remain intact"
                        );
                    }
                    for (path, expected) in &completed_outputs {
                        if missing_current.as_ref() != Some(path) {
                            assert_eq!(
                                &std::fs::read(path).unwrap(),
                                expected,
                                "refused recovery preserves completed current files"
                            );
                        }
                    }
                    assert!(
                        inner
                            .get_legacy_master_state_owners()
                            .await
                            .unwrap()
                            .is_empty()
                    );
                    continue;
                }
                if matches!(
                    fault,
                    AmbiguousChildFault::PreserveMissingDelta
                        | AmbiguousChildFault::PreserveBlankDelta
                ) {
                    let error = format!("{:#}", result.unwrap_err());
                    assert!(
                        error.contains("Invalid changes/zone page shape")
                            || error == "Missing changes/zone successor",
                        "{error}"
                    );
                    let reopened = state::SqliteStateDb::open(&database).await.unwrap();
                    assert_eq!(
                        reopened
                            .get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some("before")
                    );
                    assert_eq!(rows(&database, receipt_sql), receipt);
                    assert_eq!(rows(&database, retry_sql), retry);
                    assert_eq!(rows(&database, revision_sql), revision);
                    assert_eq!(rows(&database, mapping_sql), mappings);
                    assert_eq!(
                        rows(&database, legacy_metadata_paths_sql),
                        legacy_metadata_paths
                    );
                    assert!(rows(&database, "SELECT * FROM unattributed_legacy_proofs").is_empty());
                    assert!(
                        reopened
                            .get_metadata(&format!(
                                "{}PrimarySync",
                                crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX
                            ))
                            .await
                            .unwrap()
                            .is_none(),
                        "missing successor is not an expired cursor"
                    );
                    assert!(
                        reopened
                            .get_legacy_master_state_owners()
                            .await
                            .unwrap()
                            .is_empty()
                    );
                    assert_eq!(std::fs::read(&legacy_path).unwrap(), bytes);
                    assert_eq!(
                        std::fs::read(&legacy_xmp).unwrap(),
                        b"legacy sidecar: never adopt or replace"
                    );
                    let downloaded = reopened.get_downloaded_page(0, 20).await.unwrap();
                    assert_eq!(downloaded.len(), 1 + names.len());
                    let mut paths = std::collections::HashSet::from([legacy_path.clone()]);
                    for name in &names {
                        let current = downloaded
                            .iter()
                            .find(|row| row.id.as_ref() == name)
                            .expect("selected child published before rejected bridge");
                        let path = current.local_path.as_ref().unwrap();
                        assert!(paths.insert(path.clone()), "independent selected path");
                        assert_eq!(std::fs::read(path).unwrap(), bytes);
                        let source: Option<String> = rusqlite::Connection::open(&database).unwrap()
                            .query_row("SELECT source_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id=?1 AND version_size='original' AND local_path=?2",
                                rusqlite::params![name, path.to_str().unwrap()], |row| row.get(0)).unwrap();
                        assert_eq!(
                            source,
                            Some(data_encoding::HEXLOWER.encode(&Sha256::digest(bytes)))
                        );
                        #[cfg(feature = "xmp")]
                        if sidecars {
                            let sidecar =
                                std::fs::read_to_string(format!("{}.xmp", path.display())).unwrap();
                            assert!(sidecar.parse::<xmp_toolkit::XmpMeta>().is_ok());
                        }
                        if cycle == 0 {
                            completed_outputs.insert(path.clone(), bytes.to_vec());
                        } else {
                            assert_eq!(completed_outputs.get(path), Some(&bytes.to_vec()));
                        }
                    }
                    let requests = server.received_requests().await.unwrap().len();
                    if cycle == 0 {
                        assert_eq!(requests, names.len());
                        completed_requests = requests;
                    } else {
                        assert_eq!(
                            requests, completed_requests,
                            "invalid successor never repeats completed media"
                        );
                    }
                    continue;
                }
                let result = result.unwrap();
                let persisted = state::SqliteStateDb::open(&database).await.unwrap();
                if hardlink_fault.is_some() {
                    assert_eq!(
                        rows(&database, legacy_metadata_paths_sql),
                        legacy_metadata_paths
                    );
                    let (original, sidecar) = original_before.unwrap();
                    assert_eq!(std::fs::read(&legacy_path).unwrap(), original);
                    assert_eq!(std::fs::read(&legacy_xmp).ok(), sidecar);
                    let proofs = rows(
                        &database,
                        "SELECT * FROM unattributed_legacy_proofs ORDER BY generation",
                    );
                    assert!(retained_proofs.iter().all(|proof| proofs.contains(proof)));
                    if cycle == 1 {
                        retained_proofs = proofs;
                        retained_original = rows(
                            &database,
                            "SELECT evidence_version,original_evidence,files,prepared_at FROM unattributed_legacy",
                        );
                        retained_paths = rows(
                            &database,
                            "SELECT * FROM unattributed_legacy_paths ORDER BY path_key",
                        );
                    }
                }
                if matches!(fault, AmbiguousChildFault::PreserveSharedOriginal) {
                    let log = std::fs::read_to_string(&warning_path).unwrap();
                    assert!(!log.contains("legacy-master"));
                    assert!(!log.contains("photo.jpg"));
                }
                let label = format!(
                    "fault={fault:?} children={children} sidecars={sidecars} cycle={cycle}"
                );
                assert_eq!(
                    std::fs::read(&legacy_path).unwrap(),
                    bytes,
                    "legacy bytes {label}"
                );
                if matches!(fault, AmbiguousChildFault::PreserveSharedOriginal) {
                    assert_eq!(
                        std::fs::read(&external_alias).unwrap(),
                        bytes,
                        "alias {label}"
                    );
                    assert!(
                        !result.stats.sync_token_blocked,
                        "historical alias is not selected debt {label}"
                    );
                    assert_eq!(
                        result.stats.unattributed_legacy_assets, 0,
                        "no preparation {label}"
                    );
                }
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
                if smart_only {
                    assert_eq!(
                        std::fs::metadata(&legacy_path).unwrap().modified().unwrap(),
                        original_mtime,
                        "retained original timestamp {label}"
                    );
                    assert_eq!(
                        std::fs::metadata(&legacy_xmp).unwrap().modified().unwrap(),
                        original_sidecar_mtime,
                        "retained sidecar timestamp {label}"
                    );
                }
                assert_eq!(rows(&database, receipt_sql), receipt, "receipt {label}");
                assert_eq!(
                    rows(
                        &database,
                        "SELECT * FROM asset_metadata_paths WHERE id!='legacy-master' AND local_checksum='historical-local' ORDER BY id"
                    ),
                    historical_child_receipts,
                    "historical child receipts {label}"
                );
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
                let selected_fault = cycle == 0
                    && matches!(
                        fault,
                        AmbiguousChildFault::Cancel
                            | AmbiguousChildFault::PreserveCancel
                            | AmbiguousChildFault::StateWrite
                            | AmbiguousChildFault::PreserveStateWrite
                            | AmbiguousChildFault::PreserveCheckpointFailure
                    );
                let unknown_source = matches!(fault, AmbiguousChildFault::PreserveUnknownSource);
                let checkpoint_write_failure =
                    cycle == 0 && matches!(fault, AmbiguousChildFault::PreserveCheckpointFailure);
                let unowned_current_sidecar = cycle == 0
                    && sidecars
                    && cfg!(feature = "xmp")
                    && matches!(
                        fault,
                        AmbiguousChildFault::PreserveHistoricalClaim
                            | AmbiguousChildFault::PreserveHistoricalReplacement
                    );
                // Only current selection/source failures can veto this cursor.
                // Historical retry deadlines and preparation are preserved evidence.
                let current_complete =
                    !selected_fault && !unknown_source && !unowned_current_sidecar;
                assert_eq!(
                    persisted
                        .get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(if current_complete {
                        if smart_only {
                            "delta-after"
                        } else if hardlink_fault.is_some() && cycle >= 2 {
                            "later"
                        } else {
                            "after"
                        }
                    } else {
                        "before"
                    }),
                    "checkpoint {label}: {:?}",
                    result.stats
                );
                assert_eq!(
                    result.stats.sync_token_blocked,
                    !current_complete && !checkpoint_write_failure,
                    "current checkpoint gate {label}"
                );
                if checkpoint_write_failure {
                    assert_eq!(
                        result.stats.state_write_failures, 1,
                        "checkpoint SQL fault executed {label}"
                    );
                    assert_eq!(
                        result.failed_count, 1,
                        "checkpoint storage failure reported once {label}"
                    );
                }
                if unowned_current_sidecar {
                    assert!(
                        result.stats.exif_failures > 0,
                        "unowned sidecar veto exercised {label}"
                    );
                }
                // Without the writer these files are preservation evidence,
                // not unsatisfied output obligations or checkpoint blockers.
                if unowned_current_sidecar
                    || (!cfg!(feature = "xmp")
                        && sidecars
                        && matches!(
                            fault,
                            AmbiguousChildFault::PreserveHistoricalClaim
                                | AmbiguousChildFault::PreserveHistoricalReplacement
                        ))
                {
                    for index in 0..children {
                        assert_eq!(
                            std::fs::read(destination.join(format!("independent-{index}.jpg.xmp")))
                                .unwrap(),
                            b"independent current sidecar"
                        );
                    }
                }
                assert!(
                    inner
                        .legacy_preservations("PrimarySync")
                        .await
                        .unwrap()
                        .is_empty(),
                    "historical evidence must not create certification work {label}"
                );
                assert!(rows(&database, "SELECT * FROM unattributed_legacy_proofs").is_empty());
                if current_complete {
                    assert!(
                        !result.stats.identity_incomplete,
                        "preservation is separate from provider identity {label}"
                    );
                    if cycle >= 2 && !(relocating && cycle == 2) {
                        assert!(
                            result.stats.full_enumeration_reason.is_none(),
                            "unchanged cycle should remain incremental {label}"
                        );
                    }
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
                } else if unknown_source {
                    assert!(
                        result.stats.sync_token_blocked,
                        "unknown selected master {label}"
                    );
                    assert_eq!(inner.get_downloaded_page(0, 20).await.unwrap().len(), 1);
                    assert_eq!(result.stats.downloaded, 0);
                    assert_eq!(server.received_requests().await.unwrap().len(), 0);
                    assert_eq!(
                        rows(&database, legacy_metadata_paths_sql),
                        legacy_metadata_paths,
                        "unknown identity cannot invent historical source provenance {label}"
                    );
                    for name in &names {
                        let retained: i64 = rusqlite::Connection::open(&database).unwrap()
                            .query_row("SELECT COUNT(*) FROM provider_selection_rank_records r JOIN provider_selection_rank_pages p ON p.id=r.page_id JOIN provider_active_generations g ON g.id=p.generation WHERE r.record_name=?1 AND g.checkpoint_ready=0", [name], |row| row.get(0)).unwrap();
                        assert!(
                            retained > 0,
                            "unpaired selected child retains its exact incomplete inventory evidence {label}: {name}"
                        );
                    }
                } else {
                    let downloaded = persisted.get_downloaded_page(0, 20).await.unwrap();
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
                        let source: Option<String> = rusqlite::Connection::open(&database)
                            .unwrap()
                            .query_row(
                                "SELECT source_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id=?1 AND version_size='original' AND local_path=?2",
                                rusqlite::params![name, path.to_str().unwrap()],
                                |row| row.get(0),
                            )
                            .unwrap();
                        assert_eq!(
                            source.as_deref(),
                            Some(
                                data_encoding::HEXLOWER
                                    .encode(&Sha256::digest(bytes))
                                    .as_str()
                            ),
                            "current publication retains verified source {name} {label}"
                        );
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
                        if cycle == 2 && relocating {
                            for (path, bytes) in &completed_outputs {
                                assert_eq!(
                                    &std::fs::read(path).unwrap(),
                                    bytes,
                                    "prior child copy retained {label}"
                                );
                            }
                            assert_eq!(
                                outputs, completed_outputs,
                                "path preference changes retain already published destinations {label}"
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
                        .execute_batch("DROP TRIGGER fail_single_survivor_cursor;")
                        .unwrap();
                }
                if matches!(fault, AmbiguousChildFault::PreserveIncompleteInventory) {
                    assert_eq!(
                        inventory_requests.load(std::sync::atomic::Ordering::SeqCst),
                        0,
                        "historical preparation must not start an inventory {label}"
                    );
                } else if matches!(fault, AmbiguousChildFault::PreserveInventoryRecovery) {
                    assert_eq!(
                        inventory_requests.load(std::sync::atomic::Ordering::SeqCst),
                        0,
                        "configuration drift enumerates selected queries, not historical preparation {label}"
                    );
                }
                if cycle == 0
                    && matches!(
                        fault,
                        AmbiguousChildFault::PreserveIncompleteInventory
                            | AmbiguousChildFault::PreserveInventoryRecovery
                    )
                {
                    // Normal replay can refresh relation timestamps without a
                    // changed identity; restart must retain the discovery delay.
                    rusqlite::Connection::open(&database)
                        .unwrap()
                        .execute(
                            "UPDATE asset_master_mappings SET updated_at=updated_at+3600",
                            [],
                        )
                        .unwrap();
                }
                let requests = server.received_requests().await.unwrap().len();
                if cycle == 1 {
                    if matches!(
                        fault,
                        AmbiguousChildFault::PreserveHistoricalClaim
                            | AmbiguousChildFault::PreserveHistoricalReplacement
                    ) {
                        assert_eq!(
                            requests, 0,
                            "verified independent child copies are reused {label}"
                        );
                        let conn = rusqlite::Connection::open(&database).unwrap();
                        for (index, child) in names.iter().enumerate() {
                            let expected = destination.join(format!("independent-{index}.jpg"));
                            assert_eq!(std::fs::read(&expected).unwrap(), bytes);
                            let recorded: String = conn.query_row(
                                "SELECT local_path FROM assets WHERE library='PrimarySync' AND id=?1 AND version_size='original'",
                                [child], |row| row.get(0),
                            ).unwrap();
                            assert_eq!(std::path::PathBuf::from(recorded), expected);
                        }
                    } else if !unknown_source {
                        assert!(
                            requests
                                >= names.len()
                                    - usize::from(matches!(
                                        fault,
                                        AmbiguousChildFault::PreserveHiddenUnselected
                                    )),
                            "fresh child downloads {label}"
                        );
                    }
                    completed_requests = requests;
                }
                if cycle == 2 && matches!(fault, AmbiguousChildFault::PreserveInventoryRecovery) {
                    assert!(
                        requests >= names.len(),
                        "first current-child download after config retry {label}"
                    );
                    completed_requests = requests;
                }
                if cycle == 0 && fresh_source_recovery {
                    // Keep failed publications intact outside the selected
                    // destination so recovery must obtain verified source bytes.
                    let retained = dir.path().join("retained-unverified-publication");
                    std::fs::create_dir(&retained).unwrap();
                    for entry in std::fs::read_dir(&destination).unwrap() {
                        let path = entry.unwrap().path();
                        if path == legacy_path || path == legacy_xmp || !path.is_file() {
                            continue;
                        }
                        let bytes = std::fs::read(&path).unwrap();
                        let saved = retained.join(path.file_name().unwrap());
                        std::fs::rename(&path, &saved).unwrap();
                        retained_unverified_outputs.push((saved, bytes));
                    }
                    assert!(
                        !retained_unverified_outputs.is_empty(),
                        "fault follows real publication {label}"
                    );
                }
                for (path, bytes) in &retained_unverified_outputs {
                    assert_eq!(
                        &std::fs::read(path).unwrap(),
                        bytes,
                        "retained failed publication {label}"
                    );
                }
                if cycle >= 2
                    && !(cycle == 2
                        && matches!(fault, AmbiguousChildFault::PreserveInventoryRecovery))
                {
                    assert_eq!(requests, completed_requests, "steady network {label}");
                    if !(relocating && cycle == 2) {
                        assert_eq!(result.stats.downloaded, 0, "steady download {label}");
                        if !unknown_source {
                            assert!(
                                lookup_requests.load(std::sync::atomic::Ordering::SeqCst)
                                    - lookups_before
                                    <= 2 * names.len(),
                                "only bounded current-child source confirmation is permitted {label}"
                            );
                        }
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

#[tokio::test]
async fn run_cycle_single_survivor_first_failure_creates_retry_then_preserves_after_restart() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveFreshRetry,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_inventory_backoff_keeps_hold_then_config_change_recovers() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveInventoryRecovery,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_single_survivor_shared_original_keeps_hold() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveSharedOriginal,
        &[1],
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_legacy_hardlink_opt_in_original_and_current_qualify_after_reopen() {
    Box::pin(exercise_legacy_child_cycles_with_hardlinks(
        AmbiguousChildFault::Preserve,
        &[1],
        Some(LegacyHardlinkFault::Quiet),
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_legacy_hardlink_opt_in_disable_preserves_prepared_history_and_current_copies() {
    for fault in [
        LegacyHardlinkFault::Disable,
        LegacyHardlinkFault::DisableCurrentOnly,
    ] {
        Box::pin(exercise_legacy_child_cycles_with_hardlinks(
            AmbiguousChildFault::Preserve,
            &[1],
            Some(fault),
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_legacy_hardlink_opt_in_keeps_byte_sidecar_and_pending_write_holds() {
    for fault in [
        LegacyHardlinkFault::OriginalBytes,
        LegacyHardlinkFault::OriginalSidecarMissing,
        LegacyHardlinkFault::OriginalTemp,
        #[cfg(target_os = "linux")]
        LegacyHardlinkFault::OriginalJournal,
        LegacyHardlinkFault::CurrentMissing,
    ] {
        Box::pin(exercise_legacy_child_cycles_with_hardlinks(
            AmbiguousChildFault::Preserve,
            &[1],
            Some(fault),
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_legacy_hardlink_opt_in_incomplete_inventory_still_holds() {
    Box::pin(exercise_legacy_child_cycles_with_hardlinks(
        AmbiguousChildFault::PreserveIncompleteInventory,
        &[1],
        Some(LegacyHardlinkFault::Quiet),
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_legacy_smart_only_bridge_qualifies_after_reopen() {
    Box::pin(exercise_legacy_child_cycles_with_selection(
        AmbiguousChildFault::Preserve,
        &[1, 2],
        None,
        true,
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_legacy_smart_only_bridge_preserves_failure_guards() {
    for fault in [
        AmbiguousChildFault::PreserveCancel,
        AmbiguousChildFault::PreserveStateWrite,
        AmbiguousChildFault::PreserveCheckpointFailure,
    ] {
        Box::pin(exercise_legacy_child_cycles_with_selection(
            fault,
            &[1],
            None,
            true,
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_legacy_smart_only_query_cannot_replace_missing_delta_successor() {
    for fault in [
        AmbiguousChildFault::PreserveMissingDelta,
        AmbiguousChildFault::PreserveBlankDelta,
    ] {
        Box::pin(exercise_legacy_child_cycles_with_selection(
            fault,
            &[1],
            None,
            true,
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_legacy_opaque_mmcs_qualifies_after_reopen() {
    for signature_bytes in [21, 32] {
        for smart_only in [false, true] {
            Box::pin(exercise_legacy_child_cycles_with_provenance(
                AmbiguousChildFault::Preserve,
                &[1, 2],
                None,
                smart_only,
                Some(signature_bytes),
            ))
            .await;
        }
    }
}

#[tokio::test]
async fn run_cycle_legacy_opaque_mmcs_checkpoint_failure_stays_atomic() {
    Box::pin(exercise_legacy_child_cycles_with_provenance(
        AmbiguousChildFault::PreserveCheckpointFailure,
        &[1],
        None,
        true,
        Some(21),
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_legacy_opaque_mmcs_unknown_source_stays_unverified() {
    for smart_only in [false, true] {
        Box::pin(exercise_legacy_child_cycles_with_provenance(
            AmbiguousChildFault::PreserveUnknownSource,
            &[1],
            None,
            smart_only,
            Some(21),
        ))
        .await;
    }
}

#[tokio::test]
async fn run_cycle_historical_claim_recovers_without_reassigning_originals() {
    Box::pin(exercise_legacy_child_cycles(
        AmbiguousChildFault::PreserveHistoricalClaim,
        &[2],
    ))
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn run_cycle_historical_namespace_replacement_rolls_back_then_recovers() {
    exercise_legacy_child_cycles(AmbiguousChildFault::PreserveHistoricalReplacement, &[2]).await;
}
