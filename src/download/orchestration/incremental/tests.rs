#![cfg(test)]

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
#[cfg(feature = "xmp")]
use crate::icloud::photos::inbox::ShadowCapture;
#[cfg(feature = "xmp")]
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
#[cfg(feature = "xmp")]
use crate::state::db::account::AccountOwner;
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

#[tokio::test]
async fn issue765_cancel_backpressured_incremental_producer_with_receiver_retained() {
    const DRAIN_BOUND: Duration = Duration::from_secs(1);
    const CLEANUP_BOUND: Duration = Duration::from_secs(5);
    // Progressing pages respect the 200-record request limit.
    let session = [
        (0..100, "page-one", true),
        (100..200, "page-two", true),
        (200..250, "zone-token-after", false),
    ]
    .into_iter()
    .fold(MockPhotosFlow::new(), |flow, (range, token, more)| {
        let records = range
            .flat_map(|index| incremental_photo_records(&format!("BACKPRESSURE_{index}")))
            .collect();
        flow.changes_zone_page(records, token, more)
    })
    .build();
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("Library", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let directory = TempDir::new().unwrap();
    let mut config = test_config();
    config.directory = Arc::from(directory.path());
    config.concurrent_downloads = 1;
    let cancel = CancellationToken::new();
    let pending = Arc::new(tokio::sync::Notify::new());
    let (assets, mut producer) = super::stream_incremental_assets_for_single_unfiled_pass(
        pass,
        Arc::new(config),
        "zone-token-before".to_owned(),
        DownloadRunMode::Download,
        cancel.clone(),
        Some(pending.clone()),
    );
    let blocked = tokio::time::timeout(CLEANUP_BOUND, pending.notified()).await;
    if blocked.is_err() {
        producer.abort();
        let _ = tokio::time::timeout(CLEANUP_BOUND, &mut producer)
            .await
            .unwrap();
    }
    blocked.expect("production send must first be polled Pending");
    assert!(!producer.is_finished());
    cancel.cancel();
    let result = tokio::time::timeout(DRAIN_BOUND, &mut producer).await;
    if result.is_err() {
        producer.abort();
        let _ = tokio::time::timeout(CLEANUP_BOUND, &mut producer)
            .await
            .expect("aborted synthetic producer must join");
    }
    drop(assets);
    let summary = result
        .expect("shutdown must not wait for retained channel capacity")
        .expect("producer task joins")
        .expect("producer returns a partial summary");
    assert_eq!(
        summary.total_events, 3,
        "two queued assets and one blocked send"
    );
    assert_eq!(summary.sync_token, None);
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
    assert!(ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata(6000, 1280)));
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
fn bound_album(
    name: &str,
    container_id: Option<&str>,
    session: impl PhotosSession + 'static,
    capture: &ShadowCapture,
) -> PhotoAlbum {
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({
                "zoneName": "PrimarySync",
                "ownerRecordName": "_defaultOwner"
            })),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: container_id.map(Arc::from),
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    );
    album.set_shadow_capture(capture.clone(), "private".into());
    album
}

#[cfg(feature = "xmp")]
#[derive(Clone)]
struct AlbumSession {
    records: Arc<Vec<serde_json::Value>>,
}

#[cfg(feature = "xmp")]
#[async_trait::async_trait]
impl PhotosSession for AlbumSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
            }));
        }
        if url.contains("/records/query?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
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
            return Ok(json!({"records": records, "syncToken": "rank-selection-token"}));
        }
        if url.contains("/records/lookup?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let requested = request["records"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("lookup fixture request lacks records"))?;
            let mut records = Vec::with_capacity(requested.len());
            for record in requested {
                let name = record["recordName"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("lookup fixture request lacks recordName"))?;
                let found = self
                    .records
                    .iter()
                    .find(|record| record["recordName"].as_str() == Some(name))
                    .ok_or_else(|| anyhow::anyhow!("lookup fixture has no record {name}"))?;
                records.push(found.clone());
            }
            return Ok(json!({"records": records}));
        }
        anyhow::bail!("unexpected album fixture endpoint {url}")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[cfg(feature = "xmp")]
#[derive(Clone)]
struct QuietDeltaSession {
    expected_token: Arc<str>,
    successor: Arc<str>,
    changes_zone_calls: Arc<AtomicUsize>,
}

#[cfg(feature = "xmp")]
#[async_trait::async_trait]
impl PhotosSession for QuietDeltaSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/changes/zone?") {
            let request: serde_json::Value = serde_json::from_str(&body)?;
            let Some(zones) = request["zones"].as_array() else {
                anyhow::bail!("quiet delta request lacks zones: {body}");
            };
            anyhow::ensure!(zones.len() == 1, "quiet delta request zones length");
            let zone = &zones[0];
            anyhow::ensure!(
                zone["zoneID"]
                    == json!({"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}),
                "quiet delta used an unexpected zone: {body}"
            );
            anyhow::ensure!(
                zone["syncToken"].as_str() == Some(self.expected_token.as_ref()),
                "quiet delta used an unexpected token: {body}"
            );
            self.changes_zone_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": self.successor.as_ref(),
                    "moreComing": false,
                    "records": []
                }]
            }));
        }
        anyhow::bail!("unexpected quiet delta fixture endpoint {url} body={body}")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[cfg(feature = "xmp")]
type DestinationRow = (
    String,
    String,
    String,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
);

#[cfg(feature = "xmp")]
fn destination_rows(db: &SqliteStateDb, child: &str) -> Vec<DestinationRow> {
    let conn = db.acquire_lock("test_destination_rows").unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT pass_key,path,checksum,admitted,verified_media,verified_metadata, \
             source_checksum,local_checksum FROM provider_active_destinations \
             WHERE child=?1 ORDER BY path",
        )
        .unwrap();
    stmt.query_map([child], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
        ))
    })
    .unwrap()
    .map(|row| row.unwrap())
    .collect()
}

#[cfg(feature = "xmp")]
fn album_proof_rows(db: &SqliteStateDb) -> Vec<(PathBuf, String, Option<String>, Option<String>)> {
    let conn = db.acquire_lock("test_album_path_proofs").unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT local_path, provider_checksum, source_checksum, local_checksum \
             FROM asset_metadata_paths ORDER BY local_path",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })
    .unwrap()
    .map(|row| row.unwrap())
    .collect()
}

#[cfg(feature = "xmp")]
async fn assert_album_proofs(
    db: &SqliteStateDb,
    paths: &[PathBuf],
    source_checksum: &str,
    provider_checksum: &str,
    embed_xmp: bool,
) {
    let proofs = album_proof_rows(db);
    assert_eq!(proofs.len(), paths.len());
    for ((path, provider, source, local), expected_path) in proofs.iter().zip(paths) {
        assert_eq!(path, expected_path);
        assert_eq!(provider, provider_checksum);
        assert_eq!(source.as_deref(), Some(source_checksum));
        let actual = file::compute_sha256(path).await.unwrap();
        assert_eq!(local.as_deref(), Some(actual.as_str()));
        if embed_xmp {
            assert_ne!(source.as_deref(), Some(actual.as_str()));
        } else {
            assert_eq!(source.as_deref(), Some(actual.as_str()));
        }
    }
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
        let provider_checksum =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body));
        let source_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(&body));
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(provider_checksum);
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let owner = AccountOwner::authenticated(
            "album@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"album-fixture"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
        let master_name = records[0]["recordName"].as_str().unwrap();
        let child_name = records[1]["recordName"].as_str().unwrap();
        seed_complete_album_snapshot(&db, "family", "Family", &[(child_name, master_name)]).await;
        seed_complete_album_snapshot(&db, "trip", "Trip", &[(child_name, master_name)]).await;
        // The download planner normally derives this index from the snapshots
        // once the asset row exists. Seed both real memberships before the
        // first publication so both copies start from the same grouping.
        db.add_asset_album("PrimarySync", child_name, "Family", "icloud")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", child_name, "Trip", "icloud")
            .await
            .unwrap();
        let capture = ShadowCapture::new(db.clone(), owner.clone(), "com");
        let album_records = Arc::new(records.clone());
        let passes: Vec<_> = [("Family", "family"), ("Trip", "trip")]
            .into_iter()
            .map(|(name, id)| AlbumPass {
                kind: PassKind::Album,
                album: bound_album(
                    name,
                    Some(id),
                    AlbumSession {
                        records: Arc::clone(&album_records),
                    },
                    &capture,
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            })
            .collect();
        let mut config = test_config();
        config.directory = Arc::from(dir.path().join("media"));
        config.metadata.xmp_sidecar = true;
        config.metadata.embed_xmp = embed_xmp;
        // Two destinations share one asset row; authorize the normal metadata
        // tail so each finalized copy settles against the shared grouping.
        config.refresh_metadata = true;
        config.state_db = Some(db.clone());
        config.asset_groupings =
            crate::sync_cycle::preload_asset_groupings(config.state_db.as_deref(), &config.library)
                .await;
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
        assert_album_proofs(&db, &paths, &source_checksum, &provider_checksum, embed_xmp).await;
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
        assert_eq!(
            db.acquire_lock("test_admitted_destinations")
                .unwrap()
                .query_row(
                    "SELECT COUNT(DISTINCT pass_key) FROM provider_active_destinations \
                     WHERE asset_id=?1 AND admitted=1 AND verified_media=1",
                    [child_name],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        config.state_db = None;
        drop(passes);
        drop(capture);
        drop(db);
        let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
        config.state_db = Some(db.clone());
        config.recent = Some(10);
        config.refresh_metadata = true;
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "token-full".into(),
        };
        let capture = ShadowCapture::new(db.clone(), owner.clone(), "com");
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: bound_album(
                "",
                None,
                changes_zone_session(
                    Arc::new(AtomicUsize::new(0)),
                    vec![relation_delete_record("trip", "asset-COPIES")],
                ),
                &capture,
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        // One independently damaged sidecar must not prevent the other copy's
        // rewrite, or clear the damaged copy's durable retry marker.
        let original_sidecar = tokio::fs::read(&sidecars[0]).await.unwrap();
        tokio::fs::write(&sidecars[0], b"not parseable XMP")
            .await
            .unwrap();
        let failed = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&pass),
            Arc::new(config.clone()),
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
        let pending_path = pending[0].local_path.clone();
        let pending_checksum = pending[0].checksum.clone();
        let damaged_sidecar = tokio::fs::read(&sidecars[0]).await.unwrap();
        let healthy_sidecar = tokio::fs::read_to_string(&sidecars[1]).await.unwrap();
        let proof_before_restart = album_proof_rows(&db);
        let destinations_before_restart = destination_rows(&db, child_name);
        let mut media_before_restart = Vec::new();
        let mut sidecar_before_restart = Vec::new();
        let mut media_mtimes_before_restart = Vec::new();
        let mut sidecar_mtimes_before_restart = Vec::new();
        for (media, sidecar) in paths.iter().zip(&sidecars) {
            media_before_restart.push(tokio::fs::read(media).await.unwrap());
            sidecar_before_restart.push(tokio::fs::read(sidecar).await.unwrap());
            media_mtimes_before_restart.push(
                tokio::fs::metadata(media)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
            );
            sidecar_mtimes_before_restart.push(
                tokio::fs::metadata(sidecar)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
            );
        }
        assert_album_proofs(&db, &paths, &source_checksum, &provider_checksum, embed_xmp).await;

        // A failed explicit retry must retain its exact path debt across a real
        // owner restart before a fresh, event-free delta can repair it.
        config.state_db = None;
        drop(pass);
        drop(capture);
        drop(db);
        let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
        config.state_db = Some(db.clone());
        let capture = ShadowCapture::new(db.clone(), owner.clone(), "com");
        let reopened_pending = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(reopened_pending.len(), 1);
        assert_eq!(reopened_pending[0].local_path, pending_path);
        assert_eq!(reopened_pending[0].checksum, pending_checksum);
        assert_eq!(
            tokio::fs::read(&sidecars[0]).await.unwrap(),
            damaged_sidecar
        );
        assert_eq!(
            tokio::fs::read_to_string(&sidecars[1]).await.unwrap(),
            healthy_sidecar
        );
        assert_eq!(album_proof_rows(&db), proof_before_restart);
        assert_eq!(
            destination_rows(&db, child_name),
            destinations_before_restart
        );
        for (index, (media, sidecar)) in paths.iter().zip(&sidecars).enumerate() {
            assert_eq!(
                tokio::fs::read(media).await.unwrap(),
                media_before_restart[index]
            );
            assert_eq!(
                tokio::fs::read(sidecar).await.unwrap(),
                sidecar_before_restart[index]
            );
            assert_eq!(
                tokio::fs::metadata(media)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
                media_mtimes_before_restart[index]
            );
            assert_eq!(
                tokio::fs::metadata(sidecar)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
                sidecar_mtimes_before_restart[index]
            );
        }
        assert_album_proofs(&db, &paths, &source_checksum, &provider_checksum, embed_xmp).await;
        tokio::fs::write(&sidecars[0], original_sidecar)
            .await
            .unwrap();

        assert_eq!(failed.sync_token.as_deref(), Some("zone-token-next"));
        let repair_token = failed
            .sync_token
            .clone()
            .expect("damaged delta must return its actual successor");
        let repair_calls = Arc::new(AtomicUsize::new(0));
        let repair_pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: bound_album(
                "",
                None,
                QuietDeltaSession {
                    expected_token: repair_token.clone().into(),
                    successor: "quiet-repair-successor".into(),
                    changes_zone_calls: Arc::clone(&repair_calls),
                },
                &capture,
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: repair_token,
        };
        let repaired = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&repair_pass),
            Arc::new(config.clone()),
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
        assert_eq!(
            repair_calls.load(Ordering::SeqCst),
            1,
            "repair must consume exactly one completed empty delta"
        );
        assert_eq!(
            repaired.sync_token.as_deref(),
            Some("quiet-repair-successor")
        );
        assert_album_proofs(&db, &paths, &source_checksum, &provider_checksum, embed_xmp).await;
        let mut sidecar_mtimes = Vec::new();
        let mut sidecar_bytes = Vec::new();
        let mut media_mtimes = Vec::new();
        let mut media_bytes = Vec::new();
        for (media, sidecar) in paths.iter().zip(&sidecars) {
            if embed_xmp {
                assert_eq!(embedded_subjects(media), ["Family"]);
            } else {
                assert_eq!(tokio::fs::read(media).await.unwrap(), body);
            }
            assert_eq!(
                subjects(&tokio::fs::read_to_string(sidecar).await.unwrap()),
                ["Family"]
            );
            media_bytes.push(tokio::fs::read(media).await.unwrap());
            media_mtimes.push(
                tokio::fs::metadata(media)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
            );
            sidecar_mtimes.push(
                tokio::fs::metadata(sidecar)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
            );
            sidecar_bytes.push(tokio::fs::read(sidecar).await.unwrap());
        }
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );

        let steady_token = repaired
            .sync_token
            .clone()
            .expect("repair must return its actual successor");
        let steady_calls = Arc::new(AtomicUsize::new(0));
        let steady_pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: bound_album(
                "",
                None,
                QuietDeltaSession {
                    expected_token: steady_token.clone().into(),
                    successor: "quiet-steady-successor".into(),
                    changes_zone_calls: Arc::clone(&steady_calls),
                },
                &capture,
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: steady_token,
        };
        let steady = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&steady_pass),
            Arc::new(config),
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
        assert_eq!(
            steady_calls.load(Ordering::SeqCst),
            1,
            "steady must consume exactly one completed empty delta"
        );
        assert_eq!(steady.sync_token.as_deref(), Some("quiet-steady-successor"));
        assert_album_proofs(&db, &paths, &source_checksum, &provider_checksum, embed_xmp).await;
        for (index, (media, sidecar)) in paths.iter().zip(&sidecars).enumerate() {
            assert_eq!(tokio::fs::read(media).await.unwrap(), media_bytes[index]);
            assert_eq!(
                tokio::fs::metadata(media)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
                media_mtimes[index]
            );
            assert_eq!(
                tokio::fs::read(sidecar).await.unwrap(),
                sidecar_bytes[index]
            );
            assert_eq!(
                tokio::fs::metadata(sidecar)
                    .await
                    .unwrap()
                    .modified()
                    .unwrap(),
                sidecar_mtimes[index]
            );
        }
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        server.verify().await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn album_membership_first_xmp_and_tracked_path_removal() {
    use crate::state::MetadataRewriteStore;
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
    let first_media_mtime = tokio::fs::metadata(media_path)
        .await
        .unwrap()
        .modified()
        .unwrap();
    let first_sidecar_mtime = tokio::fs::metadata(&sidecar_path)
        .await
        .unwrap()
        .modified()
        .unwrap();
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
    // Generic baseline adoption is read-only: even an explicit drain retains debt.
    assert_eq!(
        tokio::fs::read_to_string(&sidecar_path).await.unwrap(),
        first_xmp
    );
    assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);
    assert_eq!(
        tokio::fs::metadata(media_path)
            .await
            .unwrap()
            .modified()
            .unwrap(),
        first_media_mtime
    );
    assert_eq!(
        tokio::fs::metadata(&sidecar_path)
            .await
            .unwrap()
            .modified()
            .unwrap(),
        first_sidecar_mtime
    );
    let pending = db.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].library.as_ref(), "PrimarySync");
    assert_eq!(pending[0].id.as_ref(), "asset-FIRST_ALBUM");
    assert_eq!(pending[0].local_path.as_ref(), Some(media_path));
    assert!(db.guard_primary_writer(media_path).await.is_err());
    assert_eq!(
        drain_pending_metadata_rewrites(
            db.as_ref(),
            &config.directory,
            &config.metadata,
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            config.temp_suffix.clone(),
            &CancellationToken::new(),
        )
        .await,
        1
    );
    assert_eq!(
        tokio::fs::read_to_string(&sidecar_path).await.unwrap(),
        first_xmp
    );
    assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);
    let pending = db.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].local_path.as_ref(), Some(media_path));
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].local_path, row.local_path);
    config.album_name = None;
    config.recent = Some(10);
    let change_calls = Arc::new(AtomicUsize::new(0));
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(
                change_calls.clone(),
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
            db.get_all_asset_albums("PrimarySync").await.unwrap(),
            vec![("asset-FIRST_ALBUM".to_string(), "Family".to_string())]
        );
        assert_eq!(
            tokio::fs::read_to_string(&sidecar_path).await.unwrap(),
            first_xmp
        );
        assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);
        assert_eq!(
            tokio::fs::metadata(media_path)
                .await
                .unwrap()
                .modified()
                .unwrap(),
            first_media_mtime
        );
        assert_eq!(
            tokio::fs::metadata(&sidecar_path)
                .await
                .unwrap()
                .modified()
                .unwrap(),
            first_sidecar_mtime
        );
        let pending = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.as_ref(), "asset-FIRST_ALBUM");
        assert_eq!(pending[0].local_path.as_ref(), Some(media_path));
        assert!(db.guard_primary_writer(media_path).await.is_err());
    }
    assert_eq!(change_calls.load(Ordering::SeqCst), 2);
    let reopened = SqliteStateDb::open(&dir.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        reopened.get_all_asset_albums("PrimarySync").await.unwrap(),
        vec![("asset-FIRST_ALBUM".to_string(), "Family".to_string())]
    );
    let pending = reopened.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id.as_ref(), "asset-FIRST_ALBUM");
    assert_eq!(pending[0].local_path.as_ref(), Some(media_path));
    assert!(reopened.guard_primary_writer(media_path).await.is_err());
    assert_eq!(
        tokio::fs::read_to_string(&sidecar_path).await.unwrap(),
        first_xmp
    );
    assert_eq!(tokio::fs::read(media_path).await.unwrap(), body);
    assert_eq!(
        tokio::fs::metadata(media_path)
            .await
            .unwrap()
            .modified()
            .unwrap(),
        first_media_mtime
    );
    assert_eq!(
        tokio::fs::metadata(&sidecar_path)
            .await
            .unwrap()
            .modified()
            .unwrap(),
        first_sidecar_mtime
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
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
            expected_metadata.width = before.metadata.width;
            expected_metadata.height = before.metadata.height;
            expected_metadata.duration_secs = before.metadata.duration_secs;
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
                    (
                        before.metadata.width,
                        before.metadata.height,
                        before.metadata.duration_secs
                    )
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
                    (Some(6200), Some(4500))
                } else {
                    (Some(4000), Some(3000))
                };
                assert_eq!((row.metadata.width, row.metadata.height), expected);
                let current = PhotoAsset::new(records[0].clone(), records[1].clone());
                let mut expected_metadata = (*current.shared_metadata_arc()).clone();
                expected_metadata.width = expected.0;
                expected_metadata.height = expected.1;
                expected_metadata.duration_secs = None;
                expected_metadata.refresh_hash();
                assert_eq!(row.metadata.metadata_hash, expected_metadata.metadata_hash);
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
    config.refresh_metadata = true;
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
    config.refresh_metadata = true;
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
        DownloadOutcome::PartialFailure { failed_count: 1 }
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
    assert!(
        pending.is_empty(),
        "the independent album removal rewrite completed"
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
    config.refresh_metadata = true;
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
#[derive(Clone)]
struct LiveDatesSession {
    records: Arc<Vec<serde_json::Value>>,
    expected_changes_token: Option<Arc<str>>,
    changes_successor: Arc<str>,
    query_token: Arc<str>,
    changes_calls: Arc<AtomicUsize>,
    query_calls: Arc<AtomicUsize>,
}

#[cfg(feature = "xmp")]
impl LiveDatesSession {
    fn new(
        records: Vec<serde_json::Value>,
        expected_changes_token: Option<&str>,
        changes_successor: &str,
        query_token: &str,
    ) -> Self {
        Self {
            records: Arc::new(records),
            expected_changes_token: expected_changes_token.map(Arc::from),
            changes_successor: Arc::from(changes_successor),
            query_token: Arc::from(query_token),
            changes_calls: Arc::new(AtomicUsize::new(0)),
            query_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[cfg(feature = "xmp")]
#[async_trait::async_trait]
impl PhotosSession for LiveDatesSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
            anyhow::anyhow!("invalid Live Dates request body: {error}; url={url} body={body}")
        })?;
        if url.contains("/internal/records/query/batch") {
            anyhow::ensure!(
                request["batch"].is_array(),
                "unexpected count request: {body}"
            );
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
            }));
        }
        if url.contains("/records/query?") {
            let zone = request["query"]["zoneID"]["zoneName"]
                .as_str()
                .or_else(|| request["zoneID"]["zoneName"].as_str())
                .ok_or_else(|| anyhow::anyhow!("records/query missing zone: {body}"))?;
            anyhow::ensure!(zone == "PrimarySync", "records/query wrong zone: {body}");
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            self.query_calls.fetch_add(1, Ordering::SeqCst);
            let records = if offset == 0 {
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({
                "records": records,
                "syncToken": self.query_token.as_ref()
            }));
        }
        if url.contains("/changes/zone?") {
            let zone = request["zones"][0]["zoneID"]
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("changes/zone missing zoneID: {body}"))?;
            anyhow::ensure!(
                zone.get("zoneName").and_then(|value| value.as_str()) == Some("PrimarySync"),
                "changes/zone wrong zone: {body}"
            );
            anyhow::ensure!(
                zone.get("ownerRecordName").and_then(|value| value.as_str())
                    == Some("_defaultOwner"),
                "changes/zone wrong owner: {body}"
            );
            let token = request["zones"][0]["syncToken"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("changes/zone missing token: {body}"))?;
            if let Some(expected) = &self.expected_changes_token {
                anyhow::ensure!(
                    token == expected.as_ref(),
                    "changes/zone used unexpected token {token}: {body}"
                );
            }
            self.changes_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": self.changes_successor.as_ref(),
                    "moreComing": false,
                    "records": self.records.as_ref()
                }]
            }));
        }
        if url.contains("/records/lookup?") {
            let zone = request["zoneID"]["zoneName"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("records/lookup missing zone: {body}"))?;
            anyhow::ensure!(zone == "PrimarySync", "records/lookup wrong zone: {body}");
            let requested: Vec<&str> = request["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|record| record["recordName"].as_str())
                .collect();
            anyhow::ensure!(!requested.is_empty(), "records/lookup no names: {body}");
            let selected: Vec<serde_json::Value> = self
                .records
                .iter()
                .filter(|record| {
                    requested.contains(&record["recordName"].as_str().unwrap_or_default())
                })
                .cloned()
                .collect();
            return Ok(json!({"records": selected}));
        }
        anyhow::bail!("unexpected Live Dates Photos route: {url}; body={body}")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[cfg(feature = "xmp")]
async fn seed_imported_live_photo_legacy(
    db: &SqliteStateDb,
    config: &DownloadConfig,
    old: &PhotoAsset,
    still: &[u8],
    motion: &[u8],
) -> Vec<filter::ExpectedAssetPath> {
    let expected = filter::expected_paths_for(old, config);
    let capture = filter::metadata_capture(old);
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
                sidecar_namespace: None,
                require_created_entry: false,
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
    expected
}

#[cfg(feature = "xmp")]
type LiveDatesRawRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
);

#[cfg(feature = "xmp")]
fn live_dates_raw_rows(db: &SqliteStateDb, asset: &str) -> Vec<LiveDatesRawRow> {
    let conn = db.acquire_lock("live dates raw rows").unwrap();
    let mut statement = conn
        .prepare(
            "SELECT version_size,status,local_path,checksum,local_checksum, \
                    download_checksum,metadata_write_failed_at FROM assets \
             WHERE id=?1 ORDER BY version_size",
        )
        .unwrap();
    statement
        .query_map([asset], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

#[cfg(feature = "xmp")]
fn live_dates_debt_counts(db: &SqliteStateDb, asset: &str) -> (i64, i64, i64, i64) {
    let conn = db.acquire_lock("live dates debt counts").unwrap();
    let asset_marks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM assets WHERE id=?1 AND metadata_write_failed_at IS NOT NULL",
            [asset],
            |row| row.get(0),
        )
        .unwrap();
    let path_marks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_metadata_paths \
             WHERE id=?1 AND metadata_write_failed_at IS NOT NULL",
            [asset],
            |row| row.get(0),
        )
        .unwrap();
    let source: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_metadata_paths \
             WHERE id=?1 AND source_checksum IS NOT NULL",
            [asset],
            |row| row.get(0),
        )
        .unwrap();
    let download: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM assets WHERE id=?1 AND download_checksum IS NOT NULL",
            [asset],
            |row| row.get(0),
        )
        .unwrap();
    (asset_marks, path_marks, source, download)
}

#[cfg(feature = "xmp")]
fn live_dates_receipt_count(db: &SqliteStateDb) -> i64 {
    db.acquire_lock("live dates receipt count")
        .unwrap()
        .query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[cfg(feature = "xmp")]
type LiveDatesReceiptRow = (String, String, String, Option<String>, String);

#[cfg(feature = "xmp")]
fn live_dates_receipt_snapshot(db: &SqliteStateDb) -> Vec<LiveDatesReceiptRow> {
    let conn = db.acquire_lock("live dates receipt snapshot").unwrap();
    let mut statement = conn
        .prepare(
            "SELECT version_size,native_path,provider_checksum,source_checksum,local_checksum \
             FROM download_receipts ORDER BY native_path,version_size",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn explicit_live_photo_refresh_repairs_legacy_dates_and_sidecars_then_stays_incremental() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let still = include_bytes!("../../../../tests/data/sample.heic").as_slice();
    let motion = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ".as_slice();
    let provider_still = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still));
    let provider_motion = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(motion));
    let hex_still = data_encoding::HEXLOWER.encode(&Sha256::digest(still));
    let hex_motion = data_encoding::HEXLOWER.encode(&Sha256::digest(motion));
    let old_ms = 1_700_000_000_000_i64;
    let new_ms = 1_700_000_000_123_i64;
    let new_added_ms = 1_700_000_000_789_i64;

    let server = wiremock::MockServer::start().await;
    for (name, body, content_type) in [
        ("/capture.HEIC", still, "image/heic"),
        ("/capture.MOV", motion, "video/quicktime"),
    ] {
        Mock::given(method("GET"))
            .and(path(name))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", content_type),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let mut old_records = incremental_photo_records_with_url(
        "LIVE_DATES",
        "capture.HEIC",
        &format!("{}/capture.HEIC", server.uri()),
        still.len() as u64,
    );
    old_records[0]["fields"]["itemType"] = json!({"value": "public.heic"});
    old_records[0]["fields"]["resOriginalFileType"] = json!({"value": "public.heic"});
    old_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(provider_still);
    old_records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
        "downloadURL": format!("{}/capture.MOV", server.uri()),
        "fileChecksum": provider_motion, "size": motion.len(),
    }});
    old_records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value": "com.apple.quicktime-movie"});
    old_records[1]["fields"]["assetDate"]["value"] = json!(old_ms);
    old_records[1]["fields"]["addedDate"]["value"] = json!(old_ms);
    old_records[1]["fields"]["timeZoneOffset"] = json!({"value": 0});
    let old_asset = PhotoAsset::new(old_records[0].clone(), old_records[1].clone());
    let mut new_records = old_records.clone();
    new_records[1]["fields"]["assetDate"]["value"] = json!(new_ms);
    new_records[1]["fields"]["addedDate"]["value"] = json!(new_added_ms);
    let new_asset = PhotoAsset::new(new_records[0].clone(), new_records[1].clone());
    assert_eq!(
        old_asset.metadata().metadata_hash,
        new_asset.metadata().metadata_hash
    );

    let dir = TempDir::new().unwrap();
    let media_root = dir.path().join("media");
    let db_path = dir.path().join("state.db");
    let owner = AccountOwner::authenticated(
        "live-dates@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"live-dates-fixture"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
    let capture = ShadowCapture::new(db.clone(), owner.clone(), "com");
    let initial_session = LiveDatesSession::new(old_records.clone(), None, "unused", "full-token");
    let initial_pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: bound_album("", None, initial_session.clone(), &capture),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let mut config = test_config();
    config.directory = Arc::from(media_root.as_path());
    config.state_db = Some(db.clone());
    config.metadata.xmp_sidecar = true;
    config.refresh_metadata = false;
    config.sync_mode = SyncMode::Full;
    config.concurrent_downloads = 1;
    let expected = filter::expected_paths_for(&old_asset, &config.with_pass(&initial_pass));
    assert_eq!(expected.len(), 2);
    let still_path = expected
        .iter()
        .find(|entry| entry.version_size == VersionSizeKey::Original)
        .unwrap()
        .path
        .clone();
    let motion_path = expected
        .iter()
        .find(|entry| entry.version_size == VersionSizeKey::LiveOriginal)
        .unwrap()
        .path
        .clone();
    let sidecar_for = |media: &Path| {
        media.with_file_name(format!(
            "{}.xmp",
            media.file_name().unwrap().to_str().unwrap()
        ))
    };
    let date_of = |text: &str| {
        let meta: XmpMeta = text.parse().unwrap();
        meta.property(xmp_ns::EXIF, "DateTimeOriginal")
            .unwrap()
            .value
            .clone()
    };
    let mtime =
        |path: PathBuf| async move { tokio::fs::metadata(path).await.unwrap().modified().unwrap() };

    let initial = download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&initial_pass),
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
    assert_eq!(initial.stats.downloaded, 2, "{initial:?}");
    assert_eq!(initial.stats.failed, 0, "{initial:?}");
    assert_eq!(initial.stats.exif_failures, 0, "{initial:?}");
    assert_eq!(initial.stats.state_write_failures, 0, "{initial:?}");
    assert!(initial.full_enumeration_ran);
    assert_eq!(initial.sync_token.as_deref(), Some("full-token"));
    assert_eq!(still_path, expected[0].path);
    assert_eq!(tokio::fs::read(&still_path).await.unwrap(), still);
    assert_eq!(tokio::fs::read(&motion_path).await.unwrap(), motion);

    let mut expected_proofs = vec![
        (
            still_path.clone(),
            provider_still.clone(),
            Some(hex_still.clone()),
            Some(hex_still.clone()),
        ),
        (
            motion_path.clone(),
            provider_motion.clone(),
            Some(hex_motion.clone()),
            Some(hex_motion.clone()),
        ),
    ];
    expected_proofs.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(album_proof_rows(&db), expected_proofs);
    let receipts: i64 = db
        .acquire_lock("live dates scoped receipts")
        .unwrap()
        .query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(receipts, 2);
    let receipts_before = live_dates_receipt_snapshot(db.as_ref());
    let child_name = new_records[1]["recordName"].as_str().unwrap().to_owned();
    let admitted: i64 = db
        .acquire_lock("live dates admitted destinations")
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM provider_active_destinations \
             WHERE asset_id=?1 AND admitted=1 AND verified_media=1",
            [&child_name],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(admitted, 2);

    let mut rows = db.get_downloaded_page(0, 10).await.unwrap();
    rows.sort_by(|left, right| left.version_size.as_str().cmp(right.version_size.as_str()));
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.created_at, old_asset.created());
        assert_eq!(row.added_at, Some(old_asset.added_date()));
        assert_eq!(
            row.local_path.as_deref(),
            Some(if row.version_size == VersionSizeKey::Original {
                still_path.as_path()
            } else {
                motion_path.as_path()
            })
        );
    }
    let old_sidecar = |media: &Path| tokio::fs::read_to_string(sidecar_for(media));
    let still_sidecar_before = old_sidecar(&still_path).await.unwrap();
    let motion_sidecar_before = old_sidecar(&motion_path).await.unwrap();
    let old_still_date = date_of(&still_sidecar_before);
    let old_motion_date = date_of(&motion_sidecar_before);
    assert!(old_still_date.starts_with("2023-11-14T22:13:20"));
    assert!(old_motion_date.starts_with("2023-11-14T22:13:20"));
    assert_ne!(old_still_date, "2023-11-14T22:13:20.123+00:00");

    let media_snapshot = |path: PathBuf| async move {
        (
            tokio::fs::read(&path).await.unwrap(),
            tokio::fs::metadata(&path)
                .await
                .unwrap()
                .modified()
                .unwrap(),
        )
    };
    let still_before = media_snapshot(still_path.clone()).await;
    let motion_before = media_snapshot(motion_path.clone()).await;
    let still_sidecar_mtime_before = mtime(sidecar_for(&still_path)).await;
    let motion_sidecar_mtime_before = mtime(sidecar_for(&motion_path)).await;
    let proofs_before = album_proof_rows(&db);

    drop(initial_pass);
    config.state_db = None;
    drop(capture);
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
    config.state_db = Some(db.clone());
    let capture = ShadowCapture::new(db.clone(), owner.clone(), "com");
    assert_eq!(album_proof_rows(&db), proofs_before);
    assert_eq!(tokio::fs::read(&still_path).await.unwrap(), still_before.0);
    assert_eq!(
        tokio::fs::read(&motion_path).await.unwrap(),
        motion_before.0
    );
    assert_eq!(mtime(still_path.clone()).await, still_before.1);
    assert_eq!(mtime(motion_path.clone()).await, motion_before.1);
    assert_eq!(
        tokio::fs::read_to_string(sidecar_for(&still_path))
            .await
            .unwrap(),
        still_sidecar_before
    );
    assert_eq!(
        tokio::fs::read_to_string(sidecar_for(&motion_path))
            .await
            .unwrap(),
        motion_sidecar_before
    );

    let mut token = initial.sync_token.clone().expect("initial full token");
    let no_refresh_session = LiveDatesSession::new(
        new_records.clone(),
        Some(token.as_str()),
        "quiet-no-refresh",
        "unused",
    );
    let no_refresh_pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: bound_album("", None, no_refresh_session.clone(), &capture),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: token.clone(),
    };
    config.refresh_metadata = false;
    let no_refresh = download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&no_refresh_pass),
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(no_refresh.outcome, DownloadOutcome::Success),
        "{no_refresh:?}"
    );
    assert_eq!(no_refresh.stats.downloaded, 0, "{no_refresh:?}");
    assert_eq!(no_refresh.stats.failed, 0, "{no_refresh:?}");
    assert_eq!(no_refresh.stats.exif_failures, 0, "{no_refresh:?}");
    assert!(!no_refresh.full_enumeration_ran);
    assert_eq!(no_refresh_session.changes_calls.load(Ordering::SeqCst), 1);
    drop(no_refresh_pass);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(album_proof_rows(&db), proofs_before);
    assert_eq!(tokio::fs::read(&still_path).await.unwrap(), still_before.0);
    assert_eq!(
        tokio::fs::read(&motion_path).await.unwrap(),
        motion_before.0
    );
    assert_eq!(mtime(still_path.clone()).await, still_before.1);
    assert_eq!(mtime(motion_path.clone()).await, motion_before.1);
    assert_eq!(
        tokio::fs::read_to_string(sidecar_for(&still_path))
            .await
            .unwrap(),
        still_sidecar_before
    );
    assert_eq!(
        tokio::fs::read_to_string(sidecar_for(&motion_path))
            .await
            .unwrap(),
        motion_sidecar_before
    );
    assert_eq!(
        mtime(sidecar_for(&still_path)).await,
        still_sidecar_mtime_before
    );
    assert_eq!(
        mtime(sidecar_for(&motion_path)).await,
        motion_sidecar_mtime_before
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(no_refresh.sync_token.as_deref(), Some("quiet-no-refresh"));

    let refresh_session =
        LiveDatesSession::new(new_records.clone(), None, "unused", "full-token-refresh");
    let refresh_pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: bound_album("", None, refresh_session.clone(), &capture),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    config.sync_mode = SyncMode::Full;
    config.refresh_metadata = true;
    let refreshed = download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&refresh_pass),
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(refreshed.outcome, DownloadOutcome::Success),
        "{refreshed:?}"
    );
    assert_eq!(refreshed.stats.downloaded, 0, "{refreshed:?}");
    assert_eq!(refreshed.stats.failed, 0, "{refreshed:?}");
    assert_eq!(refreshed.stats.exif_failures, 0, "{refreshed:?}");
    assert_eq!(refreshed.stats.state_write_failures, 0, "{refreshed:?}");
    assert!(refreshed.full_enumeration_ran);
    assert_eq!(refreshed.sync_token.as_deref(), Some("full-token-refresh"));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "normal explicit refresh must settle its own sidecar work"
    );
    assert_eq!(album_proof_rows(&db), proofs_before);
    assert_eq!(tokio::fs::read(&still_path).await.unwrap(), still_before.0);
    assert_eq!(
        tokio::fs::read(&motion_path).await.unwrap(),
        motion_before.0
    );
    assert_eq!(mtime(still_path.clone()).await, still_before.1);
    assert_eq!(mtime(motion_path.clone()).await, motion_before.1);
    let mut refreshed_rows = db.get_downloaded_page(0, 10).await.unwrap();
    refreshed_rows
        .sort_by(|left, right| left.version_size.as_str().cmp(right.version_size.as_str()));
    assert_eq!(refreshed_rows.len(), 2);
    for row in &refreshed_rows {
        let prior = rows
            .iter()
            .find(|prior| prior.version_size == row.version_size)
            .unwrap();
        assert_eq!(
            row.metadata.metadata_hash, prior.metadata.metadata_hash,
            "version {:?}",
            row.version_size
        );
        assert_eq!(row.created_at, new_asset.created());
        assert_eq!(row.added_at, Some(new_asset.added_date()));
    }
    assert_eq!(
        date_of(
            &tokio::fs::read_to_string(sidecar_for(&still_path))
                .await
                .unwrap()
        ),
        "2023-11-14T22:13:20.123+00:00"
    );
    assert_eq!(
        date_of(
            &tokio::fs::read_to_string(sidecar_for(&motion_path))
                .await
                .unwrap()
        ),
        "2023-11-14T22:13:20.123+00:00"
    );
    let still_after_refresh = tokio::fs::read_to_string(sidecar_for(&still_path))
        .await
        .unwrap();
    let motion_after_refresh = tokio::fs::read_to_string(sidecar_for(&motion_path))
        .await
        .unwrap();
    let still_sidecar_after_mtime = mtime(sidecar_for(&still_path)).await;
    let motion_sidecar_after_mtime = mtime(sidecar_for(&motion_path)).await;
    let unfinished: i64 = db
        .acquire_lock("live dates unfinished metadata")
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM provider_active_destinations \
             WHERE asset_id=?1 AND verified_metadata=0",
            [&child_name],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unfinished, 0);
    let receipts_after: i64 = db
        .acquire_lock("live dates receipts after refresh")
        .unwrap()
        .query_row("SELECT COUNT(*) FROM download_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(receipts_after, 2);
    drop(refresh_pass);
    token = refreshed.sync_token.clone().expect("refresh successor");
    let previous_db = Arc::downgrade(&db);
    config.state_db = None;
    drop(capture);
    drop(db);
    assert!(
        previous_db.upgrade().is_none(),
        "explicit refresh DB survived before the first quiet open"
    );

    for (index, successor) in ["quiet-one", "quiet-two"].into_iter().enumerate() {
        config.state_db = None;
        assert!(
            config.state_db.is_none(),
            "quiet {index}: prior state DB was not cleared before open"
        );
        let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
        config.state_db = Some(db.clone());
        let capture = ShadowCapture::new(db.clone(), owner.clone(), "com");
        let quiet_calls = Arc::new(AtomicUsize::new(0));
        let quiet_session = QuietDeltaSession {
            expected_token: Arc::from(token.as_str()),
            successor: Arc::from(successor),
            changes_zone_calls: Arc::clone(&quiet_calls),
        };
        let quiet_pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: bound_album("", None, quiet_session.clone(), &capture),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: token.clone(),
        };
        config.refresh_metadata = false;
        let quiet_config = Arc::new(config.clone());
        let quiet = download_photos_with_sync(
            &Client::new(),
            std::slice::from_ref(&quiet_pass),
            quiet_config.clone(),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(quiet.outcome, DownloadOutcome::Success),
            "quiet {index}: {quiet:?}"
        );
        assert_eq!(quiet.stats.downloaded, 0, "quiet {index}: {quiet:?}");
        assert_eq!(quiet.stats.failed, 0, "quiet {index}: {quiet:?}");
        assert_eq!(quiet.stats.exif_failures, 0, "quiet {index}: {quiet:?}");
        assert_eq!(
            quiet.stats.state_write_failures, 0,
            "quiet {index}: {quiet:?}"
        );
        assert!(!quiet.full_enumeration_ran);
        assert_eq!(quiet.sync_token.as_deref(), Some(successor));
        assert_eq!(quiet_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert_eq!(album_proof_rows(&db), proofs_before);
        assert_eq!(live_dates_receipt_snapshot(db.as_ref()), receipts_before);
        assert_eq!(tokio::fs::read(&still_path).await.unwrap(), still_before.0);
        assert_eq!(
            tokio::fs::read(&motion_path).await.unwrap(),
            motion_before.0
        );
        assert_eq!(mtime(still_path.clone()).await, still_before.1);
        assert_eq!(mtime(motion_path.clone()).await, motion_before.1);
        assert_eq!(
            tokio::fs::read_to_string(sidecar_for(&still_path))
                .await
                .unwrap(),
            still_after_refresh
        );
        assert_eq!(
            tokio::fs::read_to_string(sidecar_for(&motion_path))
                .await
                .unwrap(),
            motion_after_refresh
        );
        assert_eq!(
            mtime(sidecar_for(&still_path)).await,
            still_sidecar_after_mtime
        );
        assert_eq!(
            mtime(sidecar_for(&motion_path)).await,
            motion_sidecar_after_mtime
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        let previous_db = Arc::downgrade(&db);
        config.state_db = None;
        drop(quiet_config);
        drop(quiet_pass);
        drop(capture);
        drop(db);
        assert!(
            previous_db.upgrade().is_none(),
            "quiet {index}: previous DB survived before the next open"
        );
        token = quiet.sync_token.clone().expect("quiet successor");
    }

    // Contract A: imported local-only evidence remains a refusal.
    let legacy_dir = TempDir::new().unwrap();
    let legacy_db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let mut legacy_old_records = incremental_photo_records_with_url(
        "LIVE_DATES_IMPORT",
        "legacy.HEIC",
        &format!("{}/capture.HEIC", server.uri()),
        still.len() as u64,
    );
    legacy_old_records[0]["fields"]["itemType"] = json!({"value": "public.heic"});
    legacy_old_records[0]["fields"]["resOriginalFileType"] = json!({"value": "public.heic"});
    legacy_old_records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
        "downloadURL": format!("{}/capture.MOV", server.uri()),
        "fileChecksum": "provider-motion", "size": motion.len(),
    }});
    legacy_old_records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value": "com.apple.quicktime-movie"});
    legacy_old_records[1]["fields"]["assetDate"]["value"] = json!(old_ms);
    legacy_old_records[1]["fields"]["addedDate"]["value"] = json!(old_ms);
    legacy_old_records[1]["fields"]["timeZoneOffset"] = json!({"value": 0});
    let legacy_old = PhotoAsset::new(legacy_old_records[0].clone(), legacy_old_records[1].clone());
    let mut legacy_current_records = legacy_old_records.clone();
    legacy_current_records[1]["fields"]["assetDate"]["value"] = json!(new_ms);
    legacy_current_records[1]["fields"]["addedDate"]["value"] = json!(new_added_ms);
    let legacy_current = PhotoAsset::new(
        legacy_current_records[0].clone(),
        legacy_current_records[1].clone(),
    );
    assert_eq!(
        legacy_old.metadata().metadata_hash,
        legacy_current.metadata().metadata_hash
    );
    let mut legacy_config = test_config();
    legacy_config.directory = Arc::from(legacy_dir.path().join("media").as_path());
    legacy_config.state_db = Some(legacy_db.clone());
    legacy_config.sync_mode = SyncMode::Full;
    legacy_config.refresh_metadata = true;
    legacy_config.metadata.xmp_sidecar = true;
    let legacy_expected = seed_imported_live_photo_legacy(
        legacy_db.as_ref(),
        &legacy_config,
        &legacy_old,
        still,
        motion,
    )
    .await;
    assert_eq!(legacy_expected.len(), 2);
    let legacy_still_path = legacy_expected
        .iter()
        .find(|entry| entry.version_size == VersionSizeKey::Original)
        .unwrap()
        .path
        .clone();
    let legacy_motion_path = legacy_expected
        .iter()
        .find(|entry| entry.version_size == VersionSizeKey::LiveOriginal)
        .unwrap()
        .path
        .clone();
    let legacy_still_before = tokio::fs::read(&legacy_still_path).await.unwrap();
    let legacy_motion_before = tokio::fs::read(&legacy_motion_path).await.unwrap();
    let legacy_still_mtime_before = mtime(legacy_still_path.clone()).await;
    let legacy_motion_mtime_before = mtime(legacy_motion_path.clone()).await;
    let legacy_still_sidecar_before = tokio::fs::read_to_string(sidecar_for(&legacy_still_path))
        .await
        .unwrap();
    let legacy_motion_sidecar_before = tokio::fs::read_to_string(sidecar_for(&legacy_motion_path))
        .await
        .unwrap();
    let legacy_still_sidecar_mtime_before = mtime(sidecar_for(&legacy_still_path)).await;
    let legacy_motion_sidecar_mtime_before = mtime(sidecar_for(&legacy_motion_path)).await;

    let legacy_pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session_with_query_page(
                Arc::new(AtomicUsize::new(0)),
                legacy_current_records.clone(),
                json!({"records": legacy_current_records, "syncToken": "full-token"}),
                1,
            ),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let legacy_result = download_photos_with_sync(
        &Client::new(),
        std::slice::from_ref(&legacy_pass),
        Arc::new(legacy_config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    match legacy_result.outcome {
        DownloadOutcome::PartialFailure { failed_count } => {
            assert_eq!(failed_count, 3, "{legacy_result:?}")
        }
        ref other => panic!("imported local-only evidence must refuse, got {other:?}"),
    }
    assert_eq!(legacy_result.stats.downloaded, 0, "{legacy_result:?}");
    assert_eq!(legacy_result.stats.failed, 2, "{legacy_result:?}");
    assert_eq!(
        legacy_result.stats.skipped.uncertain, 1,
        "{legacy_result:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    let legacy_child = legacy_old.asset_record_name();
    let legacy_rows = live_dates_raw_rows(legacy_db.as_ref(), legacy_child);
    assert_eq!(legacy_rows.len(), 2);
    for (
        version,
        status,
        local_path,
        provider_checksum,
        local_checksum,
        download_checksum,
        marker,
    ) in &legacy_rows
    {
        let rendition = legacy_expected
            .iter()
            .find(|entry| entry.version_size.as_str() == version)
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(local_path, rendition.path.to_str().unwrap());
        assert_eq!(provider_checksum, rendition.checksum.as_ref());
        assert_eq!(
            local_checksum,
            &file::compute_sha256(&rendition.path).await.unwrap()
        );
        assert!(download_checksum.is_none());
        assert!(marker.is_some());
    }
    let (raw_asset_marks, raw_path_marks, forged_source, legacy_downloads) =
        live_dates_debt_counts(legacy_db.as_ref(), legacy_child);
    let legacy_receipts = live_dates_receipt_count(legacy_db.as_ref());
    assert_eq!(raw_asset_marks, 2, "both renditions retain raw debt");
    assert_eq!(raw_path_marks, 2, "both native paths retain raw debt");
    assert_eq!(forged_source, 0);
    assert_eq!(legacy_downloads, 0);
    assert_eq!(legacy_receipts, 0);
    assert!(
        legacy_db
            .get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "imported local-only debt must not be eligible Ordinary"
    );
    assert_eq!(
        tokio::fs::read(&legacy_still_path).await.unwrap(),
        legacy_still_before
    );
    assert_eq!(
        tokio::fs::read(&legacy_motion_path).await.unwrap(),
        legacy_motion_before
    );
    assert_eq!(
        mtime(legacy_still_path.clone()).await,
        legacy_still_mtime_before
    );
    assert_eq!(
        mtime(legacy_motion_path.clone()).await,
        legacy_motion_mtime_before
    );
    assert_eq!(
        tokio::fs::read_to_string(sidecar_for(&legacy_still_path))
            .await
            .unwrap(),
        legacy_still_sidecar_before
    );
    assert_eq!(
        tokio::fs::read_to_string(sidecar_for(&legacy_motion_path))
            .await
            .unwrap(),
        legacy_motion_sidecar_before
    );
    assert_eq!(
        mtime(sidecar_for(&legacy_still_path)).await,
        legacy_still_sidecar_mtime_before
    );
    assert_eq!(
        mtime(sidecar_for(&legacy_motion_path)).await,
        legacy_motion_sidecar_mtime_before
    );

    // Contract C: imported sidecar-only debt retains the original external drain.
    let drain_dir = TempDir::new().unwrap();
    let drain_db_path = drain_dir.path().join("state.db");
    let drain_db = Arc::new(SqliteStateDb::open(&drain_db_path).await.unwrap());
    let mut drain_old_records = incremental_photo_records_with_url(
        "LIVE_DATES_DRAIN",
        "drain.HEIC",
        &format!("{}/capture.HEIC", server.uri()),
        still.len() as u64,
    );
    drain_old_records[0]["fields"]["itemType"] = json!({"value": "public.heic"});
    drain_old_records[0]["fields"]["resOriginalFileType"] = json!({"value": "public.heic"});
    drain_old_records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
        "downloadURL": format!("{}/capture.MOV", server.uri()),
        "fileChecksum": "provider-motion", "size": motion.len(),
    }});
    drain_old_records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value": "com.apple.quicktime-movie"});
    drain_old_records[1]["fields"]["assetDate"]["value"] = json!(old_ms);
    drain_old_records[1]["fields"]["addedDate"]["value"] = json!(old_ms);
    drain_old_records[1]["fields"]["timeZoneOffset"] = json!({"value": 0});
    let drain_old = PhotoAsset::new(drain_old_records[0].clone(), drain_old_records[1].clone());
    let mut drain_current_records = drain_old_records.clone();
    drain_current_records[1]["fields"]["assetDate"]["value"] = json!(new_ms);
    drain_current_records[1]["fields"]["addedDate"]["value"] = json!(new_added_ms);
    let drain_current = PhotoAsset::new(
        drain_current_records[0].clone(),
        drain_current_records[1].clone(),
    );
    assert_eq!(
        drain_old.metadata().metadata_hash,
        drain_current.metadata().metadata_hash
    );
    let mut drain_config = test_config();
    drain_config.directory = Arc::from(drain_dir.path().join("media").as_path());
    drain_config.state_db = Some(drain_db.clone());
    drain_config.refresh_metadata = true;
    drain_config.metadata.xmp_sidecar = true;
    let drain_expected = seed_imported_live_photo_legacy(
        drain_db.as_ref(),
        &drain_config,
        &drain_old,
        still,
        motion,
    )
    .await;
    assert_eq!(drain_expected.len(), 2);
    let drain_still_path = drain_expected
        .iter()
        .find(|entry| entry.version_size == VersionSizeKey::Original)
        .unwrap()
        .path
        .clone();
    let drain_motion_path = drain_expected
        .iter()
        .find(|entry| entry.version_size == VersionSizeKey::LiveOriginal)
        .unwrap()
        .path
        .clone();
    let drain_still_before = tokio::fs::read(&drain_still_path).await.unwrap();
    let drain_motion_before = tokio::fs::read(&drain_motion_path).await.unwrap();
    let drain_still_mtime_before = mtime(drain_still_path.clone()).await;
    let drain_motion_mtime_before = mtime(drain_motion_path.clone()).await;
    for path in [&drain_still_path, &drain_motion_path] {
        assert!(
            date_of(&tokio::fs::read_to_string(sidecar_for(path)).await.unwrap())
                .starts_with("2023-11-14T22:13:20")
        );
    }
    let drain_child = drain_old.asset_record_name();
    let updated = drain_db
        .refresh_downloaded_asset_metadata(
            "PrimarySync",
            drain_child,
            (
                &filter::metadata_capture(&drain_current),
                drain_current.created(),
                Some(drain_current.added_date()),
            ),
            true,
            false,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
    assert_eq!(updated, 2);
    let drain_pending = drain_db.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(drain_pending.len(), 2);
    let mut pending_paths: Vec<PathBuf> = drain_pending
        .iter()
        .filter_map(|row| row.local_path.clone())
        .collect();
    pending_paths.sort();
    let mut expected_paths = vec![drain_still_path.clone(), drain_motion_path.clone()];
    expected_paths.sort();
    assert_eq!(pending_paths, expected_paths);
    let (_, _, drain_source, drain_download) =
        live_dates_debt_counts(drain_db.as_ref(), drain_child);
    let drain_receipts = live_dates_receipt_count(drain_db.as_ref());
    assert_eq!(drain_source, 0);
    assert_eq!(drain_download, 0);
    assert_eq!(drain_receipts, 0);
    let drain_failures = drain_pending_metadata_rewrites(
        drain_db.as_ref(),
        drain_config.directory.as_ref(),
        &drain_config.metadata,
        CaptureTimestampRepair::Preserve,
        &["PrimarySync"],
        drain_config.temp_suffix.clone(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(drain_failures, 0);
    assert!(
        drain_db
            .get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );
    let (remaining_marks, remaining_path_marks, remaining_source, remaining_download) =
        live_dates_debt_counts(drain_db.as_ref(), drain_child);
    let remaining_receipts = live_dates_receipt_count(drain_db.as_ref());
    assert_eq!(remaining_marks, 0);
    assert_eq!(remaining_path_marks, 0);
    assert_eq!(remaining_source, 0);
    assert_eq!(remaining_download, 0);
    assert_eq!(remaining_receipts, 0);
    assert_eq!(
        tokio::fs::read(&drain_still_path).await.unwrap(),
        drain_still_before
    );
    assert_eq!(
        tokio::fs::read(&drain_motion_path).await.unwrap(),
        drain_motion_before
    );
    assert_eq!(
        mtime(drain_still_path.clone()).await,
        drain_still_mtime_before
    );
    assert_eq!(
        mtime(drain_motion_path.clone()).await,
        drain_motion_mtime_before
    );
    for path in [&drain_still_path, &drain_motion_path] {
        assert_eq!(
            date_of(&tokio::fs::read_to_string(sidecar_for(path)).await.unwrap()),
            "2023-11-14T22:13:20.123+00:00"
        );
    }
    let mut drain_rows = drain_db.get_downloaded_page(0, 10).await.unwrap();
    drain_rows.sort_by(|left, right| left.version_size.as_str().cmp(right.version_size.as_str()));
    assert_eq!(drain_rows.len(), 2);
    for row in &drain_rows {
        let rendition = drain_expected
            .iter()
            .find(|entry| entry.version_size == row.version_size)
            .unwrap();
        assert_eq!(row.created_at, drain_current.created());
        assert_eq!(row.added_at, Some(drain_current.added_date()));
        assert_eq!(row.checksum.as_ref(), rendition.checksum.as_ref());
        assert!(row.download_checksum.is_none());
        assert_eq!(
            row.local_checksum.as_deref(),
            Some(
                file::compute_sha256(row.local_path.as_ref().unwrap())
                    .await
                    .unwrap()
                    .as_str()
            )
        );
    }

    server.verify().await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn collecting_rewrite_failure_retains_marker_and_advances_durable_checkpoint() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use xmp_toolkit::{XmpMeta, xmp_ns};

    let dir = TempDir::new().expect("temp dir");
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let media_path = media.join("rewrite-retry.jpg");
    let sidecar = media.join("rewrite-retry.jpg.xmp");
    let original = include_bytes!("../../../../tests/data/media/pattern.jpg");
    std::fs::write(&media_path, original).unwrap();
    let modified = std::fs::metadata(&media_path).unwrap().modified().unwrap();
    let checksum = file::compute_sha256(&media_path).await.unwrap();
    let provider_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(original));
    let unrelated = media.join("unrelated.xmp");
    std::fs::write(&unrelated, b"independent user metadata").unwrap();
    let mut metadata = crate::state::AssetMetadata {
        title: Some("fresh catalogue title".to_string()),
        ..crate::state::AssetMetadata::default()
    };
    metadata.refresh_hash();
    let record = TestAssetRecord::new("REWRITE_RETRY")
        .created_at(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap())
        .filename("rewrite-retry.jpg")
        .checksum(&provider_checksum)
        .size(original.len() as u64)
        .metadata(metadata)
        .build();
    {
        let db = SqliteStateDb::open(&database).await.unwrap();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_RETRY",
            "original",
            &media_path,
            &checksum,
            Some(&checksum),
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "REWRITE_RETRY", "original")
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "zone-token-prev")
            .await
            .unwrap();
    }
    let mut recovered_sidecar = None;
    for cycle in 0..3 {
        {
            let db = Arc::new(SqliteStateDb::open(&database).await.unwrap());
            if cycle == 0 {
                db.fail_metadata_marker_clear_for_test();
            }
            let pass = AlbumPass {
                kind: PassKind::Unfiled,
                album: changes_album(
                    "",
                    changes_zone_session(Arc::new(AtomicUsize::new(0)), Vec::new()),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            };
            let mut config = test_config();
            config.directory = Arc::from(media.clone());
            config.recent = Some(10);
            config.metadata.xmp_sidecar = true;
            config.refresh_metadata = true;
            config.state_db = Some(db.clone());
            let result = download_photos_incremental(
                &Client::new(),
                std::slice::from_ref(&pass),
                &Arc::new(config),
                "zone-token-prev",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            if cycle == 0 {
                assert!(matches!(
                    result.outcome,
                    DownloadOutcome::PartialFailure { failed_count: 1 }
                ));
            } else {
                assert!(matches!(result.outcome, DownloadOutcome::Success));
            }
            assert_eq!(result.stats.downloaded, 0, "cycle {cycle}");
            assert_eq!(result.stats.exif_failures, usize::from(cycle == 0));
            assert_eq!(result.sync_token.as_deref(), Some("zone-token-next"));
        }
        // Release production handles, including the temporary failure trigger,
        // before checking durable catalogue and publication evidence.
        let conn = rusqlite::Connection::open(&database).unwrap();
        let owned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM assets \
             WHERE library='PrimarySync' AND id='REWRITE_RETRY' AND version_size='original' \
               AND status='downloaded' AND local_path=?1 AND checksum=?2 AND download_checksum=?3 \
               AND title='fresh catalogue title' AND created_at=?4 AND metadata_hash=?5 \
               AND capture_repair_metadata_hash IS NULL AND capture_repair_output_checksum IS NULL \
               AND capture_repair_output_size IS NULL",
                rusqlite::params![
                    media_path.to_str().unwrap(),
                    provider_checksum,
                    checksum,
                    record.created_at.timestamp(),
                    record.metadata.metadata_hash
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owned, 1, "cycle {cycle}");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM assets", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let debt: Option<i64> = conn
            .query_row("SELECT metadata_write_failed_at FROM assets", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(debt.is_some(), cycle == 0, "cycle {cycle}");
        // Ordinary finalization failure clears an unconfirmed local checksum.
        // Sidecar-only recovery must not falsely vouch for the media bytes.
        assert_eq!(
            conn.query_row("SELECT local_checksum FROM assets", [], |row| row
                .get::<_, Option<String>>(0))
                .unwrap(),
            None
        );
        let publication: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_metadata_paths \
             WHERE library='PrimarySync' AND id='REWRITE_RETRY' AND version_size='original' \
               AND local_path=?1 AND provider_checksum=?2 AND download_checksum=?3 \
               AND local_checksum IS NULL AND (metadata_write_failed_at IS NOT NULL)=?4 \
               AND capture_repair_metadata_hash IS NULL AND capture_repair_output_checksum IS NULL \
               AND capture_repair_output_size IS NULL",
                rusqlite::params![
                    media_path.to_str().unwrap(),
                    provider_checksum,
                    checksum,
                    i64::from(cycle == 0)
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(publication, 1, "cycle {cycle}");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM asset_metadata_paths", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            1
        );
        // This owner returns a checkpoint candidate; the cycle owner commits it.
        assert_eq!(
            conn.query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "zone-token-prev"
        );
        drop(conn);
        assert_eq!(std::fs::read(&media_path).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&media_path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(file::compute_sha256(&media_path).await.unwrap(), checksum);
        assert_eq!(
            std::fs::read(&unrelated).unwrap(),
            b"independent user metadata"
        );
        let sidecar_bytes = std::fs::read(&sidecar).unwrap();
        let parsed: XmpMeta = std::str::from_utf8(&sidecar_bytes)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            parsed
                .localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "fresh catalogue title"
        );
        let sidecar_modified = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        if cycle == 1 {
            recovered_sidecar = Some((sidecar_bytes, sidecar_modified));
        } else if cycle == 2 {
            assert_eq!(Some((sidecar_bytes, sidecar_modified)), recovered_sidecar);
        }
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 3);
    }
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
async fn incremental_sync_retains_unproven_smaller_metadata_rewritten_file() {
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
    .expect("incremental sync should retain uncertain work");

    assert!(result.checkpoint.completed_delta_replay);
    assert!(!result.checkpoint.retained_token_rejected);
    assert!(!result.full_enumeration_ran);
    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.stats.skipped.uncertain, 1);
    let pending = db.get_pending().await.expect("read pending work");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id.as_ref(), state_id);
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
        .expect(0)
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
                .changes_zone_page(changed_records.clone(), "zone-token-next", false)
                .album_count(1)
                .query_page(changed_records, Some("rank-selection-token"))
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

    assert!(
        matches!(
            incremental.outcome,
            DownloadOutcome::PartialFailure { failed_count: 2 }
        ),
        "{incremental:?}"
    );
    assert_eq!(incremental.stats.downloaded, 0);
    assert_eq!(incremental.stats.skipped.uncertain, 1);
    assert_eq!(incremental.stats.state_write_failures, 1);
    assert!(incremental.sync_token.is_none());
    let downloaded = db.get_downloaded_page(0, 10).await.expect("downloaded row");
    assert_eq!(downloaded.len(), 1);
    assert_eq!(downloaded[0].id.as_ref(), "asset-COLLECTING_IDENTITY");
    assert_eq!(
        downloaded[0].local_path.as_deref(),
        Some(original_path.as_path())
    );
    assert_eq!(
        tokio::fs::read(&original_path).await.unwrap(),
        original_body
    );
    // Without a scoped receipt the occupied original cannot establish that a
    // different resource is safe to publish at a second path.
}

#[tokio::test]
async fn shared_delta_state_streaming_receiver_drop_keeps_completion_uncommitted() {
    const DRAIN_BOUND: Duration = Duration::from_secs(1);
    const CLEANUP_BOUND: Duration = Duration::from_secs(5);
    let dir = TempDir::new().unwrap();
    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.concurrent_downloads = 1;
    let records = ["UNCONSUMED", "QUEUED", "BLOCKED"]
        .into_iter()
        .flat_map(incremental_photo_records)
        .collect();
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(Arc::new(AtomicUsize::new(0)), records),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let pending = Arc::new(tokio::sync::Notify::new());
    let (assets, mut producer) = super::stream_incremental_assets_for_single_unfiled_pass(
        pass,
        Arc::new(config),
        "zone-token-prev".to_string(),
        DownloadRunMode::Download,
        CancellationToken::new(),
        Some(pending.clone()),
    );
    // Two assets fill the channel; the third send is actually polled Pending
    // after all three source mappings have finished their SQLite writes.
    let blocked = tokio::time::timeout(CLEANUP_BOUND, pending.notified()).await;
    if blocked.is_err() {
        producer.abort();
        let _ = tokio::time::timeout(CLEANUP_BOUND, &mut producer)
            .await
            .expect("aborted synthetic producer must join");
    }
    blocked.expect("receiver drop must follow actual durable event processing");
    assert!(!producer.is_finished());
    drop(assets);
    let result = tokio::time::timeout(DRAIN_BOUND, &mut producer).await;
    if result.is_err() {
        producer.abort();
        let _ = tokio::time::timeout(CLEANUP_BOUND, &mut producer)
            .await
            .expect("aborted synthetic producer must join");
    }
    let summary = result
        .expect("receiver closure must release the blocked asset send")
        .expect("producer task joins")
        .expect("producer returns its partial summary");
    assert_eq!(summary.sync_token, None);
    assert_eq!(summary.total_events, 3);
    assert_eq!(summary.created_count, 3);
    assert_eq!(summary.state_transition_failures, 0);
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-UNCONSUMED")
            .await
            .unwrap()
            .as_deref(),
        Some("UNCONSUMED"),
    );
    for master in ["QUEUED", "BLOCKED"] {
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", &format!("asset-{master}"))
                .await
                .unwrap()
                .as_deref(),
            Some(master),
        );
    }
    assert!(db.get_pending().await.unwrap().is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn shared_delta_state_streaming_receiver_drop_before_processing_preserves_empty_state() {
    const DRAIN_BOUND: Duration = Duration::from_secs(1);
    const CLEANUP_BOUND: Duration = Duration::from_secs(5);
    let dir = TempDir::new().unwrap();
    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(
                Arc::new(AtomicUsize::new(0)),
                incremental_photo_records("UNCONSUMED"),
            ),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let (assets, mut producer) = super::stream_incremental_assets_for_single_unfiled_pass(
        pass,
        Arc::new(config),
        "zone-token-prev".to_string(),
        DownloadRunMode::Download,
        CancellationToken::new(),
        None,
    );
    // This current-thread fixture closes the receiver before yielding to the
    // spawned incremental consumer. No event has begun durable processing.
    drop(assets);
    let result = tokio::time::timeout(DRAIN_BOUND, &mut producer).await;
    if result.is_err() {
        producer.abort();
        let _ = tokio::time::timeout(CLEANUP_BOUND, &mut producer)
            .await
            .expect("aborted synthetic producer must join");
    }
    let summary = result
        .expect("pre-work receiver closure must drain")
        .expect("producer task joins")
        .expect("producer returns an empty partial summary");
    assert_eq!(summary.sync_token, None);
    assert_eq!(summary.total_events, 0);
    assert_eq!(summary.created_count, 0);
    assert_eq!(summary.state_transition_failures, 0);
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-UNCONSUMED")
            .await
            .unwrap(),
        None,
    );
    assert!(db.get_pending().await.unwrap().is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn malformed_delta_page_rejected_by_streaming_and_collecting_consumers() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    for collecting in [true, false] {
        let server = crate::start_wiremock_or_skip!();
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
        Mock::given(method("GET"))
            .and(path("/earlier.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .expect(if collecting { 0 } else { 1 })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rejected.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .expect(0)
            .mount(&server)
            .await;
        let mut earlier = incremental_photo_records_with_url(
            "EARLIER",
            "earlier.jpg",
            &format!("{}/earlier.jpg", server.uri()),
            bytes.len() as u64,
        );
        earlier[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)));
        let dir = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap(),
        );
        db.set_metadata("sync_token:PrimarySync", "saved")
            .await
            .unwrap();
        let mut rejected = incremental_photo_records_with_url(
            "REJECTED_PAGE",
            "rejected.jpg",
            &format!("{}/rejected.jpg", server.uri()),
            bytes.len() as u64,
        );
        rejected[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            earlier[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"].clone();
        let page = |records: Vec<serde_json::Value>, token: &str, more: bool| {
            json!({
                "zones": [{"zoneID": {"zoneName": "PrimarySync"}, "records": records,
                    "syncToken": token, "moreComing": more}]
            })
        };
        let mut bad = page(rejected, "rejected-successor", false);
        bad["zones"][0]["records"].as_array_mut().unwrap().push(json!({
            "recordName": "errored-unknown", "recordType": "FutureType", "serverErrorCode": "UNKNOWN_ITEM"
        }));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                crate::test_helpers::MockPhotosSession::new()
                    .ok(page(earlier, "accepted-page", true))
                    .ok(bad),
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
            "saved",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await;
        assert!(
            result.is_err(),
            "collecting={collecting}: malformed page completed"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("saved")
        );
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        if collecting {
            assert!(db.get_all_known_ids().await.unwrap().is_empty());
        } else {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id.as_ref(), "asset-EARLIER");
            assert_eq!(
                std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
                bytes
            );
            assert_eq!(db.get_all_known_ids().await.unwrap().len(), 1);
        }
        server.verify().await;
    }
}

#[tokio::test]
async fn missing_successor_with_unaccountable_record_remains_error_in_both_incremental_paths() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    for (collecting, unknown_type) in [(true, false), (true, true), (false, false), (false, true)] {
        let server = crate::start_wiremock_or_skip!();
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
        Mock::given(method("GET"))
            .and(path("/earlier.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .expect(if collecting { 0 } else { 1 })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rejected.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .expect(0)
            .mount(&server)
            .await;
        let mut earlier = incremental_photo_records_with_url(
            "EARLIER",
            "earlier.jpg",
            &format!("{}/earlier.jpg", server.uri()),
            bytes.len() as u64,
        );
        earlier[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)));
        let dir = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap(),
        );
        db.set_metadata("sync_token:PrimarySync", "saved")
            .await
            .unwrap();
        let mut rejected = incremental_photo_records_with_url(
            "REJECTED_PAGE",
            "rejected.jpg",
            &format!("{}/rejected.jpg", server.uri()),
            bytes.len() as u64,
        );
        rejected[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            earlier[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"].clone();
        let page = |records: Vec<serde_json::Value>, token: &str, more: bool| {
            json!({
                "zones": [{"zoneID": {"zoneName": "PrimarySync"}, "records": records,
                    "syncToken": token, "moreComing": more}]
            })
        };
        let mut bad = page(rejected, "rejected-successor", false);
        bad["zones"][0]["syncToken"] = json!("");
        bad["zones"][0]["records"]
            .as_array_mut()
            .unwrap()
            .push(if unknown_type {
                json!({"recordName": "unknown-source", "recordType": "FutureType"})
            } else {
                json!({})
            });
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                crate::test_helpers::MockPhotosSession::new()
                    .ok(page(earlier, "accepted-page", true))
                    .ok(bad),
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
            "saved",
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await;
        let error = result.expect_err("unaccountable page must remain a refusal");
        assert!(
            error
                .downcast_ref::<crate::icloud::photos::error::UnaccountedChangeError>()
                .is_some(),
            "collecting={collecting}, unknown_type={unknown_type}: {error:#}"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("saved")
        );
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        if collecting {
            assert!(db.get_all_known_ids().await.unwrap().is_empty());
        } else {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id.as_ref(), "asset-EARLIER");
            assert_eq!(
                std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
                bytes
            );
            assert_eq!(db.get_all_known_ids().await.unwrap().len(), 1);
        }
        server.verify().await;
    }
}

#[tokio::test]
async fn missing_successor_retains_prior_page_in_both_incremental_paths() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    for collecting in [true, false] {
        let server = crate::start_wiremock_or_skip!();
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
        Mock::given(method("GET"))
            .and(path("/earlier.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rejected.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .expect(0)
            .mount(&server)
            .await;
        let mut earlier = incremental_photo_records_with_url(
            "EARLIER",
            "earlier.jpg",
            &format!("{}/earlier.jpg", server.uri()),
            bytes.len() as u64,
        );
        earlier[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)));
        let dir = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap(),
        );
        db.set_metadata("sync_token:PrimarySync", "saved")
            .await
            .unwrap();
        let mut rejected = incremental_photo_records_with_url(
            "REJECTED_PAGE",
            "rejected.jpg",
            &format!("{}/rejected.jpg", server.uri()),
            bytes.len() as u64,
        );
        rejected[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            earlier[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"].clone();
        let page = |records: Vec<serde_json::Value>, token: &str, more: bool| {
            json!({
                "zones": [{"zoneID": {"zoneName": "PrimarySync"}, "records": records,
                    "syncToken": token, "moreComing": more}]
            })
        };
        let mut bad = page(rejected, "rejected-successor", false);
        bad["zones"][0]["syncToken"] = json!("");

        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                crate::test_helpers::MockPhotosSession::new()
                    .ok(page(earlier, "accepted-page", true))
                    .ok(bad),
            ),
            exclude_ids: Arc::new(FxHashSet::default()),
        };
        let mut config = incremental_test_config(&dir);
        config.state_db = Some(db.clone());
        let config = Arc::new(config);
        let result = if collecting {
            download_photos_incremental_collecting_inner(
                &Client::new(),
                &[pass],
                &config,
                "saved",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
                Duration::from_secs(3600),
            )
            .await
        } else {
            download_photos_incremental(
                &Client::new(),
                &[pass],
                &config,
                "saved",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
        }
        .unwrap();
        assert!(result.stats.sync_token_blocked);
        assert_eq!(result.sync_token, None);
        assert_eq!(result.stats.downloaded, 1);

        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("saved")
        );
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_ref(), "asset-EARLIER");
        assert_eq!(
            std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
            bytes
        );
        drop(rows);
        drop(db);
        let reopened = SqliteStateDb::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("saved")
        );
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_ref(), "asset-EARLIER");
        let receipt: (String, String, String) = {
            let conn = reopened.acquire_lock("prior page receipt").unwrap();
            conn.query_row(
            "SELECT provider_checksum, local_checksum, source_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-EARLIER' AND version_size='original'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap()
        };
        let digest = Sha256::digest(bytes);
        let hex = data_encoding::HEXLOWER.encode(&digest);
        assert_eq!(
            receipt,
            (
                base64::engine::general_purpose::STANDARD.encode(digest),
                hex.clone(),
                hex
            )
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn recent_overflow_uses_current_inventory_and_preserves_checkpoint() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let body = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    for name in ["oldest", "middle", "newest"] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}.jpg")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
    }
    let records = |id: &str, date: i64| {
        let mut records = incremental_photo_records_with_url(
            id,
            &format!("{id}.jpg"),
            &format!("{}/{id}.jpg", server.uri()),
            body.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body)));
        records[1]["fields"]["assetDate"]["value"] = json!(date);
        records
    };
    let oldest = records("oldest", 1_600_000_000_000);
    let middle = records("middle", 1_650_000_000_000);
    let newest = records("newest", 1_700_000_000_000);
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("state.db");
    let mut config = test_config();
    config.directory = Arc::from(dir.path().join("media"));
    config.recent = Some(1);
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "saved-cursor".to_string(),
    };
    let previous_records = incremental_photo_records("previous");
    let previous = PhotoAsset::new(previous_records[0].clone(), previous_records[1].clone());
    let previous_path;
    {
        let db = SqliteStateDb::open(&database).await.unwrap();
        db.set_metadata("sync_token:PrimarySync", "saved-cursor")
            .await
            .unwrap();
        let pass = unused_unfiled_changes_pass();
        previous_path = seed_downloaded_metadata_asset(&db, &config, &pass, &previous).await;
    }
    let previous_bytes = tokio::fs::read(&previous_path).await.unwrap();
    let db = Arc::new(SqliteStateDb::open(&database).await.unwrap());
    config.state_db = Some(db.clone());
    let session = MockPhotosFlow::new()
        .changes_zone_page(oldest.clone(), "delta-page-1", true)
        .changes_zone_page(
            [middle.clone(), newest.clone()].concat(),
            "delta-terminal",
            false,
        )
        .album_count(3)
        .query_page([newest, middle, oldest].concat(), Some("inventory-token"))
        .build();
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album("", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    };
    let result = download_photos_with_sync(
        &Client::new(),
        &[pass],
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "{result:?}"
    );
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    let mut names: Vec<_> = rows.iter().map(|row| row.filename.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["changed.JPG", "newest.JPG"],
        "production recent selection must use current descending inventory, not shuffled delta arrival: {result:?}"
    );
    assert_eq!(
        result.sync_token, None,
        "bounded current inventory cannot accept a successor: {result:?}"
    );
    assert!(result.checkpoint.sync_token_blocked);
    assert_eq!(
        tokio::fs::read(&previous_path).await.unwrap(),
        previous_bytes
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved-cursor")
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn first_album_sync_keeps_capture_mtime_after_embedded_metadata_rewrite() {
    Box::pin(
        crate::download::orchestration::queue_projection::tests::scoped_membership_refresh_fixture(
            true,
        ),
    )
    .await;
}

#[tokio::test]
async fn v027_standalone_master_delta_holds_successor_without_shadow_fallback() {
    for collecting in [false, true] {
        let dir = TempDir::new().unwrap();
        let db = Arc::new(
            SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap(),
        );
        let key = crate::sync_cycle::sync_token_key("PrimarySync");
        db.set_metadata(&key, "before-master").await.unwrap();
        let records = incremental_photo_records("master-without-child");
        assert_eq!(records[0]["recordType"], "CPLMaster");
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album(
                "",
                changes_zone_session(Arc::new(AtomicUsize::new(0)), vec![records[0].clone()]),
            ),
            exclude_ids: Default::default(),
        }];
        let mut config = incremental_test_config(&dir);
        config.state_db = Some(db.clone());
        let config = Arc::new(config);
        let result = if collecting {
            super::download_photos_incremental_collecting(
                &Client::new(),
                &passes,
                &config,
                "before-master",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
        } else {
            download_photos_incremental(
                &Client::new(),
                &passes,
                &config,
                "before-master",
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
        }
        .unwrap();
        assert!(
            result.sync_token.is_none(),
            "standalone master has no admitted work: {result:?}"
        );
        assert!(result.checkpoint.sync_token_blocked);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(
            db.get_metadata(&key).await.unwrap().as_deref(),
            Some("before-master")
        );
        assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
    }
}
