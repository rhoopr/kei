#[cfg(feature = "xmp")]
use super::super::test_support::mock_album_with_container;
#[cfg(feature = "xmp")]
use std::collections::HashMap;
#[cfg(feature = "xmp")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite::{self, CaptureTimestampRepair};
#[cfg(feature = "xmp")]
use crate::download::pipeline::{MetadataFlags, StreamRuntime, stream_and_download_from_stream};
use crate::download::{file, filter};
use crate::icloud::photos::PhotoAsset;
use crate::state::{SqliteStateDb, VersionSizeKey};
use crate::test_helpers::{MockPhotosFlow, TestAssetRecord, mock_photo_query_page};
use crate::types::{FileMatchPolicy, RawPolicy};

use super::super::config::DownloadConfig;
use super::super::context::DownloadContext;
use super::super::dispatch::download_photos_with_sync;
#[cfg(feature = "xmp")]
use super::super::maintenance::drain_pending_metadata_rewrites;
use super::super::models::{
    DownloadControls, DownloadOutcome, DownloadReporting, DownloadRunMode, DownloadStore,
    PROVIDER_METADATA_STATE_WRITE_FAILED_REASON, SyncMode,
};
use super::super::test_support::{
    changes_album, changes_album_with_container, changes_zone_session,
    changes_zone_session_with_query_page, incremental_photo_records,
    incremental_photo_records_with_favorite, incremental_photo_records_with_url,
    incremental_test_config, mock_album, relation_delete_record, seed_complete_album_snapshot,
    seed_downloaded_metadata_asset, test_config, unused_unfiled_changes_pass,
};
use super::{download_photos_incremental, download_photos_incremental_collecting_inner};

async fn relation_removed_metadata_edit_fixture(
    db: &Arc<SqliteStateDb>,
    dir: &TempDir,
) -> (Vec<AlbumPass>, DownloadConfig, PathBuf) {
    seed_complete_album_snapshot(
        db,
        "container-vacation",
        "Vacation",
        &[("asset-ROUTED_METADATA", "ROUTED_METADATA")],
    )
    .await;
    let stored_records = incremental_photo_records_with_favorite("ROUTED_METADATA", false);
    let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
    let changed_records = incremental_photo_records_with_favorite("ROUTED_METADATA", true);
    let mut delta_records = vec![relation_delete_record(
        "container-vacation",
        "asset-ROUTED_METADATA",
    )];
    delta_records.extend(changed_records);
    let pass = AlbumPass {
        kind: PassKind::Album,
        album: changes_album_with_container(
            "Vacation",
            Some("container-vacation"),
            changes_zone_session(Arc::new(AtomicUsize::new(0)), delta_records),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(Arc::clone(db) as Arc<dyn DownloadStore>);
    let media_path = seed_downloaded_metadata_asset(db, &config, &pass, &stored_asset).await;

    (vec![pass], config, media_path)
}

#[tokio::test]
async fn incremental_multi_page_unfiled_streams_through_bounded_pipeline() {
    let session = MockPhotosFlow::new()
        .changes_zone_page(
            incremental_photo_records_with_url(
                "PAGE_ONE",
                "page1.jpg",
                "https://p01.icloud-content.com/page1.jpg",
                1024,
            ),
            "zone-token-page-1",
            true,
        )
        .changes_zone_page(
            incremental_photo_records_with_url(
                "PAGE_TWO",
                "page2.jpg",
                "https://p01.icloud-content.com/page2.jpg",
                1024,
            ),
            "zone-token-after",
            false,
        )
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("Library", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let dir = TempDir::new().expect("temp dir");
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db);
    config.concurrent_downloads = 1;

    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-before",
        DownloadControls::dry_run_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("multi-page incremental sync should succeed");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "result: {result:?}"
    );
    assert_eq!(result.sync_token, None);
    assert_eq!(result.stats.downloaded, 2);
}

// ── extract_skip_candidates tests ──────────────────────────────

// ── hash_download_config additional sensitivity tests ──────────

#[test]
fn provider_metadata_refresh_is_a_noop_for_unchanged_hash() {
    let metadata = |original, live| crate::state::MetadataCapture {
        shared: Arc::new(crate::state::AssetMetadata::default()),
        renditions: Arc::from(
            [
                (VersionSizeKey::Original, original),
                (VersionSizeKey::LiveOriginal, live),
            ]
            .map(|(key, width)| {
                (
                    key,
                    crate::state::RenditionMetadata {
                        checksum: Some(Arc::from("checksum")),
                        width: Some(width),
                        ..Default::default()
                    },
                )
            }),
        ),
    };
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset1".into())
        .or_default()
        .extend(["original".into(), "live_original".into()]);
    ctx.downloaded_metadata_hashes
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset1".into())
        .or_default()
        .extend([
            (
                "original".into(),
                metadata(4000, 1920)
                    .resolve(VersionSizeKey::Original, "checksum")
                    .compute_hash()
                    .into(),
            ),
            (
                "live_original".into(),
                metadata(4000, 1920)
                    .resolve(VersionSizeKey::LiveOriginal, "checksum")
                    .compute_hash()
                    .into(),
            ),
        ]);
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset1".into())
        .or_default()
        .extend([
            ("original".into(), "checksum".into()),
            ("live_original".into(), "checksum".into()),
        ]);

    assert!(!ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata(4000, 1920)));
    assert!(ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata(6000, 1920)));
    assert!(ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata(4000, 1280)));
    ctx.downloaded_checksums.clear();
    let unknown_hash = metadata(4000, 1920)
        .resolve(VersionSizeKey::Original, "")
        .compute_hash();
    for versions in ctx.downloaded_metadata_hashes.values_mut() {
        for hashes in versions.values_mut() {
            for hash in hashes.values_mut() {
                *hash = unknown_hash.clone().into();
            }
        }
    }
    assert!(!ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata(6000, 1280)));
}

#[tokio::test]
async fn incremental_sync_queries_zone_changes_once_per_library() {
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        incremental_photo_records("MASTER_CHANGED"),
    );
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album("album_a", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album("album_b", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());

    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("print-only incremental sync should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.sync_token, None,
        "print-only mode must not advance the sync token"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "changes/zone is zone-scoped; querying once per pass repeats the same delta"
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn album_membership_rewrites_every_finalized_path_after_restart() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    use xmp_toolkit::{OpenFileOptions, XmpFile, XmpMeta, xmp_ns};

    for embed_xmp in [false, true] {
        let server = crate::start_wiremock_or_skip!();
        let body = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        Mock::given(method("GET"))
            .and(path("/copies.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .expect(2)
            .mount(&server)
            .await;
        let mut records = incremental_photo_records_with_url(
            "COPIES",
            "copies.jpg",
            &format!("{}/copies.jpg", server.uri()),
            body.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)));
        let passes: Vec<_> = [("Family", "family"), ("Trip", "trip")]
            .into_iter()
            .map(|(name, id)| AlbumPass {
                kind: PassKind::Album,
                album: mock_album_with_container(
                    name,
                    id,
                    MockPhotosFlow::new()
                        .album_count(1)
                        .query_page(records.clone(), Some("token-full"))
                        .build(),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            })
            .collect();
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = test_config();
        config.directory = Arc::from(dir.path().join("media"));
        config.metadata.xmp_sidecar = true;
        config.metadata.embed_xmp = embed_xmp;
        config.state_db = Some(db.clone());
        let initial = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(initial.outcome, DownloadOutcome::Success),
            "{initial:?}"
        );
        assert_eq!(initial.stats.downloaded, 2);
        let paths: Vec<PathBuf> = {
            let conn = db.acquire_lock("test_finalized_paths").unwrap();
            let mut stmt = conn
                .prepare("SELECT local_path FROM asset_metadata_paths ORDER BY local_path")
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|row| PathBuf::from(row.unwrap()))
                .collect()
        };
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths
                .iter()
                .map(|path| path
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap())
                .collect::<Vec<_>>(),
            ["Family", "Trip"]
        );
        let sidecars: Vec<_> = paths
            .iter()
            .map(|path| {
                let mut name = path.file_name().unwrap().to_os_string();
                name.push(".xmp");
                path.with_file_name(name)
            })
            .collect();
        let subjects = |contents: &str| {
            let meta: XmpMeta = contents.parse().unwrap();
            let mut values: Vec<_> = meta
                .property_array(xmp_ns::DC, "subject")
                .map(|item| item.value)
                .collect();
            values.sort();
            values
        };
        let embedded_subjects = |path: &Path| {
            let mut file = XmpFile::new().unwrap();
            file.open_file(path, OpenFileOptions::default().for_read())
                .unwrap();
            let meta = file.xmp().unwrap();
            let mut values: Vec<_> = meta
                .property_array(xmp_ns::DC, "subject")
                .map(|item| item.value)
                .collect();
            values.sort();
            values
        };
        if embed_xmp {
            for path in &paths {
                assert_eq!(embedded_subjects(path), ["Family", "Trip"]);
            }
        }
        for path in &sidecars {
            assert_eq!(
                subjects(&tokio::fs::read_to_string(path).await.unwrap()),
                ["Family", "Trip"]
            );
        }
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.acquire_lock("test_registered_paths")
                .unwrap()
                .query_row("SELECT COUNT(*) FROM asset_metadata_paths", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            2
        );
        config.state_db = None;
        drop(db);
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        config.state_db = Some(db.clone());
        config.recent = Some(10);
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(
                    Arc::new(AtomicUsize::new(0)),
                    vec![relation_delete_record("trip", "asset-COPIES")],
                ),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        // One independently damaged sidecar must not prevent the other copy's
        // rewrite, or clear the damaged copy's durable retry marker.
        let original_sidecar = tokio::fs::read(&sidecars[0]).await.unwrap();
        tokio::fs::write(&sidecars[0], b"not parseable XMP")
            .await
            .unwrap();
        let failed = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config.clone()),
            "token-full",
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
        assert_eq!(
            tokio::fs::read(&sidecars[0]).await.unwrap(),
            b"not parseable XMP"
        );
        assert_eq!(
            subjects(&tokio::fs::read_to_string(&sidecars[1]).await.unwrap()),
            ["Family"]
        );
        let pending = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].local_path.as_ref(), Some(&paths[0]));
        tokio::fs::write(&sidecars[0], original_sidecar)
            .await
            .unwrap();
        let repaired = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config.clone()),
            "token-full",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(repaired.outcome, DownloadOutcome::Success),
            "{repaired:?}"
        );
        assert_eq!(repaired.stats.downloaded, 0);
        let mut mtimes = Vec::new();
        for (media, sidecar) in paths.iter().zip(&sidecars) {
            if embed_xmp {
                assert_eq!(embedded_subjects(media), ["Family"]);
            } else {
                assert_eq!(tokio::fs::read(media).await.unwrap(), body);
            }
            let checksum = file::compute_sha256(media).await.unwrap();
            let recorded: String = db
                .acquire_lock("test_path_fingerprints")
                .unwrap()
                .query_row(
                    "SELECT local_checksum FROM asset_metadata_paths WHERE local_path = ?1",
                    [media.to_string_lossy()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(recorded, checksum);
            assert_eq!(
                subjects(&tokio::fs::read_to_string(sidecar).await.unwrap()),
                ["Family"]
            );
            mtimes.push(
                tokio::fs::metadata(sidecar)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
            );
        }
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        let steady = download_photos_incremental(
            &Client::new(),
            &[pass],
            &Arc::new(config),
            "token-full",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(steady.outcome, DownloadOutcome::Success),
            "{steady:?}"
        );
        assert_eq!(steady.stats.downloaded, 0);
        for (sidecar, before) in sidecars.iter().zip(mtimes) {
            assert_eq!(
                tokio::fs::metadata(sidecar)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
                before
            );
        }
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn album_membership_first_xmp_and_tracked_path_removal() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let server = crate::start_wiremock_or_skip!();
    let body = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    Mock::given(method("GET"))
        .and(path("/album.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let mut records = incremental_photo_records_with_url(
        "FIRST_ALBUM",
        "album.jpg",
        &format!("{}/album.jpg", server.uri()),
        body.len() as u64,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)));
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let dir = TempDir::new().unwrap();
    let db = Arc::new(
        SqliteStateDb::open(&dir.path().join("state.db"))
            .await
            .unwrap(),
    );
    let mut config = test_config();
    config.directory = Arc::from(dir.path().join("media"));
    config.album_name = Some(Arc::from("Trip"));
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    assert!(
        db.get_all_asset_albums("PrimarySync")
            .await
            .unwrap()
            .is_empty()
    );
    let first = stream_and_download_from_stream(
        &Client::new(),
        futures_util::stream::iter(vec![Ok(asset.clone())]),
        &Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None).deferring_metadata_drain(),
    )
    .await
    .unwrap();
    assert_eq!(first.downloaded, 1);
    assert_eq!(first.exif_failures, 0);
    let row = db.get_downloaded_page(0, 10).await.unwrap().remove(0);
    let media_path = row.local_path.as_ref().unwrap();
    let mut sidecar_name = media_path.file_name().unwrap().to_os_string();
    sidecar_name.push(".xmp");
    let sidecar_path = media_path.with_file_name(sidecar_name);
    let subjects = |contents: &str| {
        let xmp: XmpMeta = contents.parse().expect("parse actual sidecar");
        let mut names: Vec<_> = xmp
            .property_array(xmp_ns::DC, "subject")
            .map(|value| value.value)
            .collect();
        names.sort();
        names
    };
    let first_xmp = tokio::fs::read_to_string(&sidecar_path).await.unwrap();
    assert_eq!(
        subjects(&first_xmp),
        ["Trip"],
        "inspect before any rewrite drain"
    );
    assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);

    seed_complete_album_snapshot(&db, "trip", "Trip", &[("asset-FIRST_ALBUM", "FIRST_ALBUM")])
        .await;
    seed_complete_album_snapshot(
        &db,
        "family",
        "Family",
        &[("asset-FIRST_ALBUM", "FIRST_ALBUM")],
    )
    .await;
    config.album_name = Some(Arc::from("Family"));
    let second = stream_and_download_from_stream(
        &Client::new(),
        futures_util::stream::iter(vec![Ok(asset)]),
        &Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();
    assert_eq!(second.downloaded, 0);
    assert_eq!(second.exif_failures, 0);
    assert_eq!(
        subjects(&tokio::fs::read_to_string(&sidecar_path).await.unwrap()),
        ["Family", "Trip"]
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );

    config.album_name = None;
    config.recent = Some(10);
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(
                Arc::new(AtomicUsize::new(0)),
                vec![relation_delete_record("trip", "asset-FIRST_ALBUM")],
            ),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    for _ in 0..2 {
        let removed = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &Arc::new(config.clone()),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(removed.outcome, DownloadOutcome::Success),
            "{removed:?}"
        );
        assert_eq!(removed.stats.downloaded, 0);
        assert_eq!(
            subjects(&tokio::fs::read_to_string(&sidecar_path).await.unwrap()),
            ["Family"]
        );
        assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path, row.local_path);
    }
}

#[tokio::test]
async fn collecting_album_membership_failure_replays_without_redownloading() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let records = incremental_photo_records("COLLECTED_GROUPING");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    seed_complete_album_snapshot(
        db.as_ref(),
        "container-vacation",
        "Vacation",
        &[("asset-COLLECTED_GROUPING", "COLLECTED_GROUPING")],
    )
    .await;
    let change_calls = Arc::new(AtomicUsize::new(0));
    let pass = AlbumPass {
        kind: PassKind::Album,
        album: changes_album_with_container(
            "Vacation",
            Some("container-vacation"),
            changes_zone_session(Arc::clone(&change_calls), records),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.recent = Some(10);
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let media_path = seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &asset).await;
    let config = Arc::new(config);
    let media_before = tokio::fs::read(&media_path)
        .await
        .expect("read seeded media");

    db.fail_asset_album_writes_for_test();
    let failed = download_photos_incremental(
        &Client::new(),
        std::slice::from_ref(&pass),
        &config,
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("membership failure should return a partial result");

    assert!(matches!(
        failed.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(failed.stats.state_write_failures, 1);
    assert_eq!(failed.stats.downloaded, 0);
    assert!(
        db.get_all_asset_albums("PrimarySync")
            .await
            .expect("read missing album membership")
            .is_empty(),
        "the injected failure must leave the compatibility relationship absent"
    );

    db.allow_asset_album_writes_for_test();
    for cycle in 2..=3 {
        let recovered = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &config,
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("grouping replay cycle {cycle} failed: {error}"));

        assert!(matches!(recovered.outcome, DownloadOutcome::Success));
        assert_eq!(recovered.stats.state_write_failures, 0);
        assert_eq!(
            recovered.stats.downloaded, 0,
            "cycle {cycle} must not redownload landed media"
        );
        assert_eq!(
            db.get_all_asset_albums("PrimarySync")
                .await
                .expect("read recovered album membership"),
            vec![("COLLECTED_GROUPING".to_string(), "Vacation".to_string())],
            "cycle {cycle} must leave one idempotent relationship"
        );
    }
    assert_eq!(change_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        tokio::fs::read(&media_path)
            .await
            .expect("read stable media"),
        media_before,
        "grouping replay must not change landed media"
    );
}

#[tokio::test]
async fn filtered_original_replacement_refreshes_metadata_and_advances_checkpoint() {
    for (item_type, swapped_original_only) in [
        ("public.jpeg", false),
        ("com.apple.quicktime-movie", false),
        ("public.jpeg", true),
    ] {
        for recent in [None, Some(10)] {
            let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
            let dir = TempDir::new().unwrap();
            let mut records = incremental_photo_records_with_favorite("REPLACED", false);
            records[0]["fields"]["itemType"] = json!({"value": item_type});
            records[0]["fields"]["resOriginalFileType"] = json!({"value": item_type});
            records[0]["fields"]["resOriginalWidth"] = json!({"value": 1920});
            records[0]["fields"]["resOriginalHeight"] = json!({"value": 1080});
            records[1]["fields"]["duration"] = json!({"value": 12.5});
            if swapped_original_only {
                records[0]["fields"]["resOriginalAltRes"] = json!({"value": {
                    "downloadURL": "https://p01.icloud-content.com/alternative",
                    "size": 2048, "fileChecksum": "raw-alternative",
                }});
                records[0]["fields"]["resOriginalAltFileType"] =
                    json!({"value": "com.adobe.raw-image"});
                records[0]["fields"]["resOriginalAltWidth"] = json!({"value": 6000});
                records[0]["fields"]["resOriginalAltHeight"] = json!({"value": 4500});
            }
            let stored = PhotoAsset::new(records[0].clone(), records[1].clone())
                .with_state_record_name(Arc::from("asset-REPLACED"));
            let pass = AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album(
                    "",
                    changes_zone_session(Arc::new(AtomicUsize::new(0)), records.clone()),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            };
            let mut config = test_config();
            config.directory = Arc::from(dir.path());
            config.recent = recent;
            config.file_match_policy = FileMatchPolicy::NameId7;
            config.state_db = Some(db.clone());
            config.raw_policy = RawPolicy::PreferRaw;
            config.alternative = false;
            seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored).await;
            let mut initial = db.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(initial.len(), 1);
            let before = initial.remove(0);
            if swapped_original_only {
                assert_eq!(before.version_size, VersionSizeKey::Original);
                assert_eq!(before.checksum.as_ref(), "raw-alternative");
                assert_eq!(
                    (before.metadata.width, before.metadata.height),
                    (Some(6000), Some(4500))
                );
                for suffix in ["Res", "FileType", "Width", "Height"] {
                    records[0]["fields"]
                        .as_object_mut()
                        .unwrap()
                        .remove(&format!("resOriginalAlt{suffix}"));
                }
            }
            config.media.photos = false;
            config.media.videos = false;
            if !swapped_original_only {
                records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                    json!("provider-replacement");
            }
            records[1]["fields"]["isFavorite"] = json!({"value": 1});
            records[1]["fields"]["captionEnc"] =
                json!({"value": "replacement title", "type": "STRING"});
            let changed = PhotoAsset::new(records[0].clone(), records[1].clone());
            assert_eq!(
                changed.metadata_arc(VersionSizeKey::Original).width,
                Some(1920)
            );
            let mut expected_metadata = (*changed.metadata_arc(VersionSizeKey::Original)).clone();
            expected_metadata.width = None;
            expected_metadata.height = None;
            expected_metadata.duration_secs = None;
            expected_metadata.refresh_hash();
            let mut token = "zone-token-prev".to_owned();
            for cycle in 0..2 {
                if cycle == 1 {
                    db.fail_provider_metadata_refresh_for_test();
                }
                let pass = AlbumPass {
                    kind: PassKind::Unfiled,
                    album: changes_album(
                        "",
                        changes_zone_session(Arc::new(AtomicUsize::new(0)), records.clone()),
                    ),
                    exclude_ids: Arc::new(FxHashSet::default()),
                };
                let result = download_photos_incremental(
                    &Client::new(),
                    &[pass],
                    &Arc::new(config.clone()),
                    &token,
                    DownloadControls::download_hidden(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                assert!(
                    matches!(result.outcome, DownloadOutcome::Success),
                    "{item_type}, swapped={swapped_original_only}, recent={recent:?}, cycle={cycle}: {result:?}"
                );
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(result.stats.state_write_failures, 0);
                assert!(!result.stats.sync_token_blocked);
                assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
                token = result.sync_token.unwrap();
                let rows = db.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), 1);
                let row = &rows[0];
                assert!(row.metadata.is_favorite);
                assert_eq!(row.metadata.title.as_deref(), Some("replacement title"));
                assert_eq!(row.metadata.metadata_hash, expected_metadata.metadata_hash);
                assert_eq!(
                    (
                        row.metadata.width,
                        row.metadata.height,
                        row.metadata.duration_secs
                    ),
                    (None, None, None)
                );
                assert_eq!(
                    row.metadata.metadata_hash,
                    Some(row.metadata.compute_hash())
                );
                assert_eq!(row.status, before.status);
                assert_eq!(row.local_path, before.local_path);
                assert_eq!(row.checksum, before.checksum);
                assert_eq!(row.local_checksum, before.local_checksum);
                assert_eq!(row.download_checksum, before.download_checksum);
                assert_eq!(row.downloaded_at, before.downloaded_at);
                assert_eq!(
                    tokio::fs::read(row.local_path.as_ref().unwrap())
                        .await
                        .unwrap(),
                    vec![0; row.size_bytes as usize]
                );
                assert!(db.get_pending().await.unwrap().is_empty());
                assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn raw_policy_incremental_metadata_refresh_captures_unknown_bytes_without_blocking() {
    for policy in [RawPolicy::PreferRaw, RawPolicy::PreferJpeg] {
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut records = incremental_photo_records_with_favorite("RAW_DELTA", false);
        let (original_type, alternative_type) = if policy == RawPolicy::PreferRaw {
            ("public.jpeg", "com.adobe.raw-image")
        } else {
            ("com.adobe.raw-image", "public.jpeg")
        };
        records[0]["fields"]["resOriginalFileType"] = json!({"value": original_type});
        records[0]["fields"]["resOriginalAltRes"] = json!({"value": {
            "downloadURL": "https://p01.icloud-content.com/alternative",
            "size": 2048, "fileChecksum": "alternative",
        }});
        records[0]["fields"]["resOriginalAltFileType"] = json!({"value": alternative_type});
        records[0]["fields"]["resOriginalWidth"] = json!({"value": 4000});
        records[0]["fields"]["resOriginalHeight"] = json!({"value": 3000});
        records[0]["fields"]["resOriginalAltWidth"] = json!({"value": 6000});
        records[0]["fields"]["resOriginalAltHeight"] = json!({"value": 4500});
        let stored = PhotoAsset::new(records[0].clone(), records[1].clone())
            .with_state_record_name(Arc::from("asset-RAW_DELTA"));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), records.clone()),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.raw_policy = policy;
        config.alternative = true;
        config.recent = Some(10);
        config.state_db = Some(db.clone());
        seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored).await;
        let before = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(before.len(), 2);
        config.raw_policy = RawPolicy::AsIs;
        config.media.photos = false;
        config.exclude_asset_ids = Arc::new(
            ["RAW_DELTA".into(), "asset-RAW_DELTA".into()]
                .into_iter()
                .collect(),
        );
        records[0]["fields"]["resOriginalAltWidth"] = json!({"value": 6200});
        let mut hashes = Vec::new();
        for cycle in 0..4 {
            if cycle == 1 {
                db.fail_provider_metadata_refresh_for_test();
            }
            if cycle == 2 {
                db.acquire_lock("allow raw delta refresh")
                    .unwrap()
                    .execute_batch("DROP TRIGGER fail_provider_metadata_refresh")
                    .unwrap();
                records[0]["fields"]["resOriginalAltRes"]["value"]["fileChecksum"] =
                    json!("unknown");
                records[0]["fields"]["resOriginalAltWidth"] = json!({"value": 9999});
                records[1]["fields"]["isFavorite"] = json!({"value": 1});
            }
            if cycle == 3 {
                db.fail_provider_metadata_refresh_for_test();
            }
            let pass = AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album(
                    "",
                    changes_zone_session(Arc::new(AtomicUsize::new(0)), records.clone()),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            };
            let result = download_photos_incremental(
                &Client::new(),
                &[pass],
                &Arc::new(config.clone()),
                "zone-token-prev",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.stats.downloaded, 0);
            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "{result:?}"
            );
            assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
            assert_eq!(result.stats.state_write_failures, 0);
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            for row in &rows {
                let initial = before
                    .iter()
                    .find(|initial| initial.version_size == row.version_size)
                    .unwrap();
                let expected = if row.version_size == VersionSizeKey::Original {
                    if cycle < 2 {
                        (Some(6200), Some(4500))
                    } else {
                        (None, None)
                    }
                } else {
                    (Some(4000), Some(3000))
                };
                assert_eq!((row.metadata.width, row.metadata.height), expected);
                assert_eq!(row.metadata.is_favorite, cycle >= 2);
                if cycle >= 2 && row.version_size == VersionSizeKey::Original {
                    assert_eq!(row.metadata.duration_secs, None);
                }
                assert_eq!(
                    row.metadata.metadata_hash,
                    Some(row.metadata.compute_hash())
                );
                assert_eq!(row.local_path, initial.local_path);
                assert_eq!(row.checksum, initial.checksum);
                assert_eq!(row.local_checksum, initial.local_checksum);
                assert_eq!(
                    tokio::fs::read(row.local_path.as_ref().unwrap())
                        .await
                        .unwrap(),
                    vec![0; row.size_bytes as usize]
                );
            }
            let current: Vec<_> = rows
                .iter()
                .map(|row| row.metadata.metadata_hash.clone())
                .collect();
            if cycle == 0 || cycle == 2 {
                hashes = current;
            } else {
                assert_eq!(hashes, current);
            }
            assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn collecting_metadata_edit_refreshes_catalogue_without_duplicate_for_both_path_policies() {
    for policy in [
        FileMatchPolicy::NameSizeDedupWithSuffix,
        FileMatchPolicy::NameId7,
    ] {
        let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let mut stored_records = incremental_photo_records_with_favorite("METADATA_EDIT", false);
        stored_records[0]["fields"]["resOriginalWidth"] = json!({"value": 5712});
        stored_records[0]["fields"]["resOriginalHeight"] = json!({"value": 4284});
        stored_records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
            "downloadURL": "https://p01.icloud-content.com/changed.MOV",
            "size": 2048,
            "fileChecksum": "motion-checksum",
        }});
        stored_records[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value": "com.apple.quicktime-movie"});
        stored_records[0]["fields"]["resOriginalVidComplWidth"] = json!({"value": 1744});
        stored_records[0]["fields"]["resOriginalVidComplHeight"] = json!({"value": 1308});
        stored_records[1]["fields"]["duration"] = json!({"value": 0});
        stored_records[1]["fields"]["vidComplDurValue"] = json!({"value": 2300000000_u64});
        stored_records[1]["fields"]["vidComplDurScale"] = json!({"value": 1000000000});
        let stored_asset = PhotoAsset::new(stored_records[0].clone(), stored_records[1].clone());
        // Only the companion dimensions change; the original hash must remain current.
        let mut changed_records = stored_records;
        changed_records[0]["fields"]["resOriginalVidComplWidth"] = json!({"value": 1920});
        changed_records[0]["fields"]["resOriginalVidComplHeight"] = json!({"value": 1440});
        let changed_asset = PhotoAsset::new(changed_records[0].clone(), changed_records[1].clone());
        assert_eq!(
            stored_asset
                .metadata_arc(VersionSizeKey::Original)
                .metadata_hash,
            changed_asset
                .metadata_arc(VersionSizeKey::Original)
                .metadata_hash
        );
        assert_ne!(
            stored_asset
                .metadata_arc(VersionSizeKey::LiveOriginal)
                .metadata_hash,
            changed_asset
                .metadata_arc(VersionSizeKey::LiveOriginal)
                .metadata_hash
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), changed_records),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.file_match_policy = policy;
        config.recent = Some(10);
        config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
        let media_path =
            seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;
        let initial = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(initial.len(), 2);
        let config = Arc::new(config);
        db.fail_provider_metadata_refresh_for_test();
        let failed = download_photos_incremental(
            &Client::new(),
            std::slice::from_ref(&pass),
            &config,
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(
            failed.outcome,
            DownloadOutcome::PartialFailure { .. }
        ));
        assert_eq!(failed.stats.downloaded, 0);
        assert_eq!(failed.sync_token, None);
        assert!(failed.stats.sync_token_blocked);
        assert!(failed.stats.state_write_failures > 0);
        assert_eq!(
            failed.stats.sync_token_blocked_reason,
            Some(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON)
        );
        for row in db.get_downloaded_page(0, 10).await.unwrap() {
            let before = initial
                .iter()
                .find(|before| before.version_size == row.version_size)
                .unwrap();
            assert_eq!(
                row.metadata.metadata_hash,
                stored_asset.metadata_arc(row.version_size).metadata_hash
            );
            assert_eq!(row.metadata.width, before.metadata.width);
            assert_eq!(row.metadata.height, before.metadata.height);
            assert_eq!(row.metadata.duration_secs, before.metadata.duration_secs);
            assert_eq!(row.status, before.status);
            assert_eq!(row.local_path, before.local_path);
            assert_eq!(row.checksum, before.checksum);
            assert_eq!(row.local_checksum, before.local_checksum);
            assert_eq!(row.download_checksum, before.download_checksum);
            assert_eq!(
                tokio::fs::read(row.local_path.as_ref().unwrap())
                    .await
                    .unwrap(),
                vec![0u8; row.size_bytes as usize]
            );
        }
        assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
        db.acquire_lock("test_allow_provider_metadata_refresh")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_provider_metadata_refresh")
            .unwrap();

        for cycle in 0..2 {
            if cycle == 1 {
                db.fail_provider_metadata_refresh_for_test();
            }
            let result = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &config,
                "zone-token-prev",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .expect("metadata-only incremental sync should complete");

            assert!(matches!(result.outcome, DownloadOutcome::Success));
            assert_eq!(result.stats.downloaded, 0, "policy: {policy:?}");
            assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
            assert_eq!(result.stats.state_write_failures, 0);
            let refreshed = db
                .get_downloaded_page(0, 10)
                .await
                .expect("read refreshed rows");
            assert_eq!(refreshed.len(), 2);
            assert_ne!(
                refreshed[0].metadata.metadata_hash,
                refreshed[1].metadata.metadata_hash
            );
            for row in &refreshed {
                let before = initial
                    .iter()
                    .find(|before| before.version_size == row.version_size)
                    .unwrap();
                assert_eq!(
                    row.metadata.metadata_hash,
                    changed_asset.metadata_arc(row.version_size).metadata_hash
                );
                let (width, height, duration) = match row.version_size {
                    VersionSizeKey::Original => (5712, 4284, 0.0),
                    VersionSizeKey::LiveOriginal => (1920, 1440, 2.3),
                    other => panic!("unexpected rendition {other:?}"),
                };
                assert_eq!(
                    (
                        row.metadata.width,
                        row.metadata.height,
                        row.metadata.duration_secs
                    ),
                    (Some(width), Some(height), Some(duration))
                );
                assert_eq!(row.status, before.status);
                assert_eq!(row.local_path, before.local_path);
                assert_eq!(row.checksum, before.checksum);
                assert_eq!(row.local_checksum, before.local_checksum);
                assert_eq!(row.download_checksum, before.download_checksum);
                assert_eq!(
                    tokio::fs::read(row.local_path.as_ref().unwrap())
                        .await
                        .unwrap(),
                    vec![0u8; row.size_bytes as usize]
                );
            }
            assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
            assert!(media_path.exists(), "policy: {policy:?}");
            assert_eq!(
                std::fs::read_dir(media_path.parent().expect("media parent"))
                    .expect("read media parent")
                    .count(),
                2,
                "metadata-only edit must not create a suffixed duplicate under {policy:?}"
            );
        }
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn collecting_unrouted_metadata_edit_refreshes_catalogue_and_queues_rewrite() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let (passes, mut config, media_path) = relation_removed_metadata_edit_fixture(&db, &dir).await;
    config.metadata.xmp_sidecar = true;
    db.fail_metadata_marker_clear_for_test();

    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("unrouted metadata edit should remain durable");

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    let refreshed = db
        .get_downloaded_page(0, 1)
        .await
        .expect("read refreshed row")
        .remove(0);
    assert!(refreshed.metadata.is_favorite);
    assert!(media_path.exists());
    let sidecar_name = format!(
        "{}.xmp",
        media_path
            .file_name()
            .expect("media path has filename")
            .to_string_lossy()
    );
    assert!(media_path.with_file_name(sidecar_name).exists());
    assert_eq!(
        db.get_pending_metadata_rewrites(10)
            .await
            .expect("read rewrite queue")
            .len(),
        1,
        "the configured rewrite marker must remain visible when clearing it fails"
    );
    assert!(
        db.get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-ROUTED_METADATA",
            &["container-vacation"],
        )
        .await
        .expect("read album memberships")
        .is_empty(),
        "the relation removal must leave the changed asset unrouted"
    );
}

#[tokio::test]
async fn collecting_unrouted_metadata_write_failure_preserves_incremental_checkpoint() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let (passes, mut config, _) = relation_removed_metadata_edit_fixture(&db, &dir).await;
    config.metadata.set_exif_rating = true;
    db.fail_provider_metadata_refresh_for_test();

    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("failed unrouted metadata edit should return a safe partial result");

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 2 }
    ));
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.sync_token, None);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON)
    );
    let unchanged = db
        .get_downloaded_page(0, 1)
        .await
        .expect("read unchanged row")
        .remove(0);
    assert!(!unchanged.metadata.is_favorite);
    let pending = db
        .get_pending_metadata_rewrites(10)
        .await
        .expect("read rewrite queue");
    assert_eq!(
        pending.len(),
        1,
        "the independent album removal must retain retry evidence"
    );
    assert!(
        !pending[0].metadata.is_favorite,
        "failed provider metadata must not leak into the rewrite"
    );
    assert!(
        db.get_all_asset_albums("PrimarySync")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn collecting_unchanged_metadata_is_a_production_path_noop() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let records = incremental_photo_records_with_favorite("METADATA_UNCHANGED", false);
    let stored_asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(Arc::new(AtomicUsize::new(0)), records),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.recent = Some(10);
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let media_path =
        seed_downloaded_metadata_asset(db.as_ref(), &config, &pass, &stored_asset).await;
    db.fail_provider_metadata_refresh_for_test();

    let result = download_photos_incremental(
        &Client::new(),
        std::slice::from_ref(&pass),
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("unchanged metadata should not call the failing refresh operation");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.state_write_failures, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(media_path.exists());
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .expect("read rewrite queue")
            .is_empty()
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn collecting_zero_download_cycle_drains_metadata_rewrite_batch() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let media_path = dir.path().join("rewrite-only.jpg");
    tokio::fs::write(&media_path, b"existing media")
        .await
        .expect("seed media");
    let record = TestAssetRecord::new("REWRITE_ONLY")
        .filename("rewrite-only.jpg")
        .checksum("provider-checksum")
        .size(14)
        .build();
    db.upsert_seen(&record).await.expect("seed state row");
    db.mark_downloaded(
        "PrimarySync",
        "REWRITE_ONLY",
        "original",
        &media_path,
        "local-checksum",
        None,
    )
    .await
    .expect("mark downloaded");
    db.record_metadata_write_failure("PrimarySync", "REWRITE_ONLY", "original")
        .await
        .expect("queue rewrite");

    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(Arc::new(AtomicUsize::new(0)), Vec::new()),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.recent = Some(10);
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

    let result = download_photos_incremental(
        &Client::new(),
        std::slice::from_ref(&pass),
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("rewrite-only collecting cycle should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(media_path.with_file_name("rewrite-only.jpg.xmp").exists());
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .expect("read rewrite queue")
            .is_empty()
    );
}

#[tokio::test]
async fn rejected_capture_upserts_never_dispatch_streaming_or_collecting_tasks() {
    let server = crate::start_wiremock_or_skip!();
    for collecting in [false, true] {
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut records = incremental_photo_records_with_url(
            "REJECTED",
            "capture.jpg",
            &format!("{}/capture.jpg", server.uri()),
            1024,
        );
        records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
            "downloadURL": format!("{}/capture.mov", server.uri()),
            "fileChecksum": "provider-motion", "size": 1024,
        }});
        records[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value": "com.apple.quicktime-movie"});
        let old = PhotoAsset::new(records[0].clone(), records[1].clone());
        let path = dir.path().join("historical.jpg");
        tokio::fs::write(&path, b"historical media").await.unwrap();
        let record = TestAssetRecord::new(old.asset_record_name())
            .created_at(old.created())
            .added_at(old.added_date())
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .metadata(old.metadata().clone())
            .build();
        db.import_adopt(&record, &path, "local-original", 16, None)
            .await
            .unwrap();
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            &record.id,
            (
                &crate::state::MetadataCapture {
                    shared: Arc::clone(&record.metadata),
                    renditions: Arc::from([(
                        record.version_size,
                        crate::state::RenditionMetadata {
                            checksum: Some(Arc::from(record.checksum.as_ref())),
                            width: record.metadata.width,
                            height: record.metadata.height,
                            duration_secs: record.metadata.duration_secs,
                        },
                    )]),
                },
                record.created_at,
                record.added_at,
            ),
            false,
            true,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let mut pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                crate::state::db::MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .remove(0);
        pending.capture_repair_receipt = db
            .record_capture_repair_prepared(&pending, "prepared", 2048)
            .await
            .unwrap();
        records[1]["fields"]["assetDate"]["value"] = json!(1_700_086_400_123_i64);
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), records),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = incremental_test_config(&dir);
        config.state_db = Some(db.clone());
        config.recent = collecting.then_some(10);
        let result = download_photos_incremental(
            &Client::new(),
            &[pass],
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "collecting={collecting}"
        );
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.state_write_failures, 2);
        assert_eq!(result.stats.skipped.on_disk, 0);
        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { .. }
        ));
        let recovered = db
            .get_pending_metadata_rewrites_page_for_queue(
                crate::state::db::MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].capture_repair_receipt,
            pending.capture_repair_receipt
        );
        assert_eq!(recovered[0].asset.created_at, record.created_at);
        assert_eq!(recovered[0].asset.checksum, record.checksum);
        assert_eq!(
            recovered[0].asset.local_checksum.as_deref(),
            Some("local-original")
        );
        assert_eq!(
            recovered[0].asset.local_path.as_deref(),
            Some(path.as_path())
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"historical media");
        assert!(db.get_pending().await.unwrap().is_empty());
        assert!(db.get_failed().await.unwrap().is_empty());
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn explicit_live_photo_refresh_repairs_legacy_dates_and_sidecars_then_stays_incremental() {
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let still = include_bytes!("../../../../tests/data/sample.heic").as_slice();
    let motion = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ".as_slice();
    let mut records = incremental_photo_records_with_url(
        "LIVE_DATES",
        "capture.HEIC",
        "https://p01.icloud-content.com/capture.HEIC",
        still.len() as u64,
    );
    records[0]["fields"]["itemType"] = json!({"value": "public.heic"});
    records[0]["fields"]["resOriginalFileType"] = json!({"value": "public.heic"});
    records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
        "downloadURL": "https://p01.icloud-content.com/capture.MOV",
        "fileChecksum": "provider-motion", "size": motion.len(),
    }});
    records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value": "com.apple.quicktime-movie"});
    records[1]["fields"]["timeZoneOffset"] = json!({"value": 0});
    let old = PhotoAsset::new(records[0].clone(), records[1].clone());
    records[1]["fields"]["assetDate"]["value"] = json!(1_700_000_000_123_i64);
    records[1]["fields"]["addedDate"]["value"] = json!(1_700_000_000_789_i64);
    let current = PhotoAsset::new(records[0].clone(), records[1].clone());
    assert_eq!(
        old.metadata().metadata_hash,
        current.metadata().metadata_hash
    );
    let session = changes_zone_session_with_query_page(
        Arc::new(AtomicUsize::new(0)),
        records.clone(),
        json!({"records": records, "syncToken": "full-token"}),
        1,
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album("", session.clone()),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.sync_mode = SyncMode::Full;
    config.refresh_metadata = true;
    config.metadata.xmp_sidecar = true;
    let expected = filter::expected_paths_for(&old, &config);
    assert_eq!(expected.len(), 2);
    let capture = filter::metadata_capture(&old);
    for rendition in &expected {
        let bytes = if rendition.version_size == VersionSizeKey::Original {
            still
        } else {
            motion
        };
        tokio::fs::create_dir_all(rendition.path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&rendition.path, bytes).await.unwrap();
        let checksum = file::compute_sha256(&rendition.path).await.unwrap();
        let record = TestAssetRecord::new(old.asset_record_name())
            .version_size(rendition.version_size)
            .filename(rendition.path.file_name().unwrap().to_str().unwrap())
            .created_at(old.created())
            .added_at(old.added_date())
            .checksum(&rendition.checksum)
            .size(rendition.size)
            .metadata(capture.resolve(rendition.version_size, &rendition.checksum))
            .build();
        db.import_adopt(&record, &rendition.path, &checksum, rendition.size, None)
            .await
            .unwrap();
        let outcome =
            metadata_rewrite::write_download_metadata(metadata_rewrite::MetadataWriteRequest {
                final_path: &rendition.path,
                embed_path: None,
                expected_embed_fingerprint: None,
                source_checksum: None,
                sidecar_path: Some(&rendition.path),
                payload: Arc::new(filter::MetadataPayload::from_metadata(old.metadata())),
                created_local: old.metadata().capture_local(old.created()),
                flags: MetadataFlags::XMP_SIDECAR,
                capture_timestamp_repair: CaptureTimestampRepair::Preserve,
                temp_suffix: ".seed",
            })
            .await;
        assert!(!outcome.any_failed());
    }
    let before = db.get_downloaded_page(0, 10).await.unwrap();
    let mut sidecars = HashMap::new();
    let mut full_query_count = 0;
    for refresh in [true, false] {
        config.refresh_metadata = refresh;
        let result = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&pass),
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{:?}",
            result.stats
        );
        assert_eq!(result.full_enumeration_ran, refresh);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.state_write_failures, 0);
        assert_eq!(result.stats.exif_failures, 0);
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(
            result.sync_token.as_deref(),
            Some(if refresh {
                "full-token"
            } else {
                "zone-token-next"
            })
        );
        if refresh {
            full_query_count = session.records_query_count();
            assert!(full_query_count > 0);
            assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 2);
            // The explicit sync owner drains only after the full producer has finished.
            assert_eq!(
                drain_pending_metadata_rewrites(
                    db.as_ref(),
                    &config.metadata,
                    CaptureTimestampRepair::Preserve,
                    &["PrimarySync"],
                    config.temp_suffix.clone(),
                    &CancellationToken::new(),
                )
                .await,
                0
            );
        }
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            let prior = before
                .iter()
                .find(|prior| prior.version_size == row.version_size)
                .unwrap();
            assert_eq!(row.created_at, current.created());
            assert_eq!(row.added_at, Some(current.added_date()));
            assert_eq!(row.metadata.metadata_hash, prior.metadata.metadata_hash);
            assert_eq!(row.local_path, prior.local_path);
            assert_eq!(row.local_checksum, prior.local_checksum);
            assert_eq!(row.download_checksum, prior.download_checksum);
            assert_eq!(row.checksum, prior.checksum);
            assert_eq!(row.downloaded_at, prior.downloaded_at);
            let path = row.local_path.as_ref().unwrap();
            assert_eq!(
                tokio::fs::read(path).await.unwrap(),
                if row.version_size == VersionSizeKey::Original {
                    still
                } else {
                    motion
                }
            );
            let sidecar = path.with_file_name(format!(
                "{}.xmp",
                path.file_name().unwrap().to_str().unwrap()
            ));
            let text = tokio::fs::read_to_string(sidecar).await.unwrap();
            if refresh {
                let xmp: XmpMeta = text.parse().unwrap();
                assert_eq!(
                    xmp.property(xmp_ns::EXIF, "DateTimeOriginal")
                        .unwrap()
                        .value,
                    "2023-11-14T22:13:20.123+00:00"
                );
                sidecars.insert(path.clone(), text);
            } else {
                assert_eq!(sidecars.get(path), Some(&text));
            }
        }
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(session.records_query_count(), full_query_count);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: result.sync_token.unwrap(),
        };
    }
    assert_eq!(session.changes_zone_calls.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn collecting_rewrite_failure_retains_marker_and_advances_durable_checkpoint() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let media_path = dir.path().join("rewrite-retry.jpg");
    tokio::fs::write(&media_path, b"existing media")
        .await
        .expect("seed media");
    let mut metadata = crate::state::AssetMetadata {
        title: Some("fresh catalogue title".to_string()),
        ..crate::state::AssetMetadata::default()
    };
    metadata.refresh_hash();
    let record = TestAssetRecord::new("REWRITE_RETRY")
        .filename("rewrite-retry.jpg")
        .checksum("provider-checksum")
        .size(14)
        .metadata(metadata)
        .build();
    db.upsert_seen(&record).await.expect("seed state row");
    db.mark_downloaded(
        "PrimarySync",
        "REWRITE_RETRY",
        "original",
        &media_path,
        "local-checksum",
        None,
    )
    .await
    .expect("mark downloaded");
    db.record_metadata_write_failure("PrimarySync", "REWRITE_RETRY", "original")
        .await
        .expect("queue rewrite");
    db.fail_metadata_marker_clear_for_test();

    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(Arc::new(AtomicUsize::new(0)), Vec::new()),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.recent = Some(10);
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

    let result = download_photos_incremental(
        &Client::new(),
        std::slice::from_ref(&pass),
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("rewrite failure should remain a durable partial result");

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.exif_failures, 1);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert!(media_path.with_file_name("rewrite-retry.jpg.xmp").exists());
    let pending = db
        .get_pending_metadata_rewrites(10)
        .await
        .expect("read rewrite queue");
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].metadata.title.as_deref(),
        Some("fresh catalogue title"),
        "the retained marker must reference the durable fresh catalogue state"
    );
}

#[tokio::test]
async fn incremental_changed_asset_in_selected_album_routes_to_album_path() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(
        &db,
        "container-vacation",
        "Vacation",
        &[("asset-MASTER_CHANGED", "MASTER_CHANGED")],
    )
    .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        incremental_photo_records("MASTER_CHANGED"),
    );
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
    config.folder_structure = "Unfiled".to_string();
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("album-routed incremental sync should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
    assert_eq!(
        album_rows,
        vec![("asset-MASTER_CHANGED".to_string(), "Vacation".to_string())],
        "selected album asset should route through the album pass"
    );
}

#[tokio::test]
async fn incremental_changed_asset_without_selected_album_routes_to_unfiled() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session(
        Arc::clone(&calls),
        incremental_photo_records("MASTER_CHANGED"),
    );
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
    config.folder_structure = "Unfiled".to_string();
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("unfiled-routed incremental sync should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let album_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
    assert!(
        album_rows.is_empty(),
        "asset outside selected albums should route only through the unfiled pass"
    );
}

#[tokio::test]
async fn incremental_multi_album_unfiled_uses_membership_without_album_enumeration() {
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    seed_complete_album_snapshot(&db, "container-vacation", "Vacation", &[]).await;
    let changes_calls = Arc::new(AtomicUsize::new(0));
    let session = changes_zone_session_with_query_page(
        Arc::clone(&changes_calls),
        incremental_photo_records("MASTER_CHANGED"),
        mock_photo_query_page("VACATION_EXISTING", Some("album-token")),
        1,
    );
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: changes_album_with_container(
                "Vacation",
                Some("container-vacation"),
                session.clone(),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        unused_unfiled_changes_pass(),
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.folder_structure = "Unfiled".to_string();
    config.folder_structure_albums = Arc::from("{album}");
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);

    let result = download_photos_incremental(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("incremental routing should not enumerate album passes");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        changes_calls.load(Ordering::SeqCst),
        1,
        "incremental sync should still read the zone delta once"
    );
    assert_eq!(
        session.count_query_count(),
        0,
        "membership-backed incremental routing must not count albums before planning downloads"
    );
    assert_eq!(
        session.records_query_count(),
        0,
        "membership-backed incremental routing must not enumerate albums before planning downloads"
    );
}

#[tokio::test]
async fn incremental_sync_skips_smaller_metadata_rewritten_file() {
    let mut records = incremental_photo_records_with_url(
        "INCREMENTAL_METADATA_REWRITTEN",
        "incremental-rewritten.jpg",
        "https://p01.icloud-content.com/incremental-rewritten.jpg",
        8,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!("ck_incremental_metadata_rewritten");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());

    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    db.upsert_asset_master_mapping("PrimarySync", asset.asset_record_name(), asset.id())
        .await
        .expect("seed asset/master mapping");
    let dir = TempDir::new().expect("temp dir");
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let target_path = filter::expected_paths_for(&asset, &config)
        .into_iter()
        .next()
        .expect("asset should derive a path")
        .path;
    tokio::fs::create_dir_all(target_path.parent().expect("target parent"))
        .await
        .expect("create target parent");
    tokio::fs::write(&target_path, b"shorter")
        .await
        .expect("seed metadata-rewritten file");
    let local_checksum = file::compute_sha256(&target_path)
        .await
        .expect("hash metadata-rewritten file");
    let state_id = asset.asset_record_name();
    let state_record = TestAssetRecord::new(state_id)
        .filename("incremental-rewritten.jpg")
        .checksum("ck_incremental_metadata_rewritten")
        .size(8)
        .build();
    db.upsert_seen(&state_record)
        .await
        .expect("seed downloaded row");
    db.mark_downloaded(
        "PrimarySync",
        state_id,
        VersionSizeKey::Original.as_str(),
        &target_path,
        &local_checksum,
        Some("download-checksum-before-metadata"),
    )
    .await
    .expect("seed metadata-rewritten state");

    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album(
            "Library",
            MockPhotosFlow::new()
                .changes_zone_page(records, "zone-token-next", false)
                .build(),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let result = download_photos_incremental_collecting_inner(
        &Client::new(),
        &passes,
        &Arc::new(config),
        "zone-token-prev",
        DownloadControls::download_hidden(),
        CancellationToken::new(),
        Duration::ZERO,
    )
    .await
    .expect("incremental sync should skip the verified local file");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.downloaded, 0);
    assert_eq!(result.stats.failed, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
    assert_eq!(tokio::fs::read(&target_path).await.unwrap(), b"shorter");
    assert!(
        !target_path
            .with_file_name("incremental-rewritten-8.jpg")
            .exists(),
        "incremental sync must not create a size-suffixed duplicate"
    );
}

#[tokio::test]
async fn incremental_collecting_paired_asset_preserves_child_state_identity() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let original_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
    let changed_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x00];
    Mock::given(method("GET"))
        .and(path("/collecting-original.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(original_body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/collecting-changed.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(changed_body.clone())
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let original_url = format!("{}/collecting-original.jpg", server.uri());
    let changed_url = format!("{}/collecting-changed.jpg", server.uri());
    let mut original_records = incremental_photo_records_with_url(
        "COLLECTING_IDENTITY",
        "collecting.jpg",
        &original_url,
        original_body.len() as u64,
    );
    original_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&original_body)));
    let mut changed_records = incremental_photo_records_with_url(
        "COLLECTING_IDENTITY",
        "collecting.jpg",
        &changed_url,
        changed_body.len() as u64,
    );
    changed_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&changed_body)));

    let full_passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album(
            "Library",
            MockPhotosFlow::new()
                .album_count(1)
                .query_page(original_records, Some("zone-token-full"))
                .build(),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    let dir = TempDir::new().expect("temp dir");
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.state_db = Some(db.clone());

    let full = download_photos_with_sync(
        &Client::new(),
        &full_passes,
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("full sync should store the child identity");
    assert!(matches!(full.outcome, DownloadOutcome::Success));
    assert_eq!(full.stats.downloaded, 1);
    let full_rows = db.get_downloaded_page(0, 10).await.expect("full state row");
    assert_eq!(full_rows.len(), 1);
    assert_eq!(full_rows[0].id.as_ref(), "asset-COLLECTING_IDENTITY");
    let original_path = full_rows[0]
        .local_path
        .clone()
        .expect("full state row has a local path");

    let incremental_passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album(
            "Library",
            MockPhotosFlow::new()
                .changes_zone_page(changed_records, "zone-token-next", false)
                .build(),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];
    config.recent = Some(10);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-full".to_string(),
    };

    let incremental = download_photos_with_sync(
        &Client::new(),
        &incremental_passes,
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("collecting incremental sync should keep the child identity");

    assert!(matches!(incremental.outcome, DownloadOutcome::Success));
    assert_eq!(incremental.stats.downloaded, 1);
    assert_eq!(incremental.stats.failed, 0);
    assert_eq!(incremental.sync_token.as_deref(), Some("zone-token-next"));
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 1);
    assert_eq!(downloaded[0].id.as_ref(), "asset-COLLECTING_IDENTITY");
    assert_ne!(
        downloaded[0].local_path.as_deref(),
        Some(original_path.as_path())
    );
    assert!(
        original_path.exists(),
        "the prior provider bytes must remain on disk"
    );
}
