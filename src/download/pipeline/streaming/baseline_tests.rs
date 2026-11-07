//! Baseline matching through the real streaming producer, transfer and SQLite owners.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use futures_util::stream;
use rusqlite::{Connection, types::Value};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{StreamRuntime, stream_and_download_from_stream};
use crate::commands::{AlbumPass, PassKind};
use crate::download::pipeline::outcome::StreamingResult;
use crate::download::{DownloadConfig, DownloadControls, DownloadReporting, DownloadRunMode};
use crate::icloud::photos::PhotoAsset;
use crate::state::{DownloadStateStore, SqliteStateDb, VersionSizeKey};
use crate::test_helpers::TestAssetRecord;

const STILL: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/media/pattern.jpg"
));
const MOTION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/media/pattern.mov"
));

fn generation(tag: u8) -> String {
    base64::engine::general_purpose::STANDARD.encode([tag; 32])
}

fn asset(server: &MockServer, companion: bool) -> PhotoAsset {
    let mut fields = json!({
        "filenameEnc": {"value":"IMG_0001.JPG","type":"STRING"},
        "itemType":{"value":"public.jpeg"},
        "resOriginalRes":{"value":{
            "size":STILL.len(),"downloadURL":format!("{}/still",server.uri()),
            "fileChecksum":generation(1)
        }},
        "resOriginalFileType":{"value":"public.jpeg"}
    });
    if companion {
        fields["resOriginalVidComplRes"] = json!({"value":{
            "size":MOTION.len(),"downloadURL":format!("{}/motion",server.uri()),
            "fileChecksum":generation(2)
        }});
        fields["resOriginalVidComplFileType"] = json!({"value":"com.apple.quicktime-movie"});
    }
    PhotoAsset::new(
        json!({"recordName":"current-master","fields":fields}),
        json!({"recordName":"current-child","fields":{
            "assetDate":{"value":1736899200000.0},"addedDate":{"value":1736899200000.0}
        }}),
    )
    .with_source_zone(Arc::from("SharedSync"))
}

fn config(root: &Path, db: Arc<SqliteStateDb>) -> Arc<DownloadConfig> {
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(root);
    config.folder_structure = "none".into();
    config.library = "SharedSync".into();
    config.state_db = Some(db);
    config.concurrent_downloads = 1;
    Arc::new(config)
}

async fn run(
    root: &Path,
    db: Arc<SqliteStateDb>,
    assets: Vec<PhotoAsset>,
    mode: DownloadRunMode,
) -> StreamingResult {
    let count = assets.len() as u64;
    stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(assets.into_iter().map(Ok::<_, anyhow::Error>)),
        &config(root, db),
        DownloadControls::new(mode, DownloadReporting::hidden()),
        count,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap()
}

async fn seed(db_path: &Path, media: &Path) {
    std::fs::create_dir_all(media.parent().unwrap()).unwrap();
    std::fs::write(media, STILL).unwrap();
    let db = SqliteStateDb::open(db_path).await.unwrap();
    let record = TestAssetRecord::new("historical-master")
        .library("PrimarySync")
        .filename("IMG_0001.JPG")
        .checksum(&generation(1))
        .size(STILL.len() as u64)
        .build();
    db.upsert_seen(&record).await.unwrap();
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(STILL));
    db.mark_verified_download(
        "PrimarySync",
        "historical-master",
        "original",
        media,
        &digest,
        Some(&digest),
        false,
    )
    .await
    .unwrap();
}

fn old_rows(db_path: &Path) -> Vec<Vec<Value>> {
    let conn = Connection::open(db_path).unwrap();
    let mut values = Vec::new();
    for table in ["assets", "asset_metadata_paths"] {
        let mut statement = conn
            .prepare(&format!(
                "SELECT * FROM {table} WHERE library='PrimarySync' AND id='historical-master'"
            ))
            .unwrap();
        let columns = statement.column_count();
        values.extend(
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get(column))
                        .collect::<Result<Vec<Value>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
    }
    values
}

fn current_rows(db_path: &Path) -> Vec<(String, String, String)> {
    let conn = Connection::open(db_path).unwrap();
    conn.prepare("SELECT version_size,status,COALESCE(local_path,'') FROM assets WHERE library='SharedSync' AND id='current-child' ORDER BY version_size").unwrap()
        .query_map([], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap()
        .collect::<Result<Vec<_>,_>>().unwrap()
}

async fn mount(server: &MockServer, still_downloads: u64, companion_downloads: u64) {
    Mock::given(method("GET"))
        .and(path("/still"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(STILL)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(still_downloads)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/motion"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MOTION)
                .insert_header("content-type", "video/quicktime"),
        )
        .expect(companion_downloads)
        .mount(server)
        .await;
}

async fn open_current_pass(directory: &Path) -> (Arc<SqliteStateDb>, Arc<DownloadConfig>) {
    let (db, mut configured) =
        crate::download::pipeline::test_support::owned_download_fixture(directory, "SharedSync")
            .await;
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: configured
            .primary_layout_source
            .as_ref()
            .unwrap()
            .as_ref()
            .clone(),
        exclude_ids: Arc::default(),
    };
    configured.directory = directory.join("photos").into();
    configured.folder_structure = "none".into();
    (db, Arc::new(configured.with_pass(&pass)))
}

fn named_current_asset(server: &MockServer, filename: &str, companion: bool) -> PhotoAsset {
    let source = asset(server, companion);
    let mut pair = source.observed_pair().unwrap().clone();
    pair["records"][0]["fields"]["filenameEnc"]["value"] = json!(filename);
    pair["records"][1]["fields"]["masterRef"] = json!({"type":"REFERENCE","value":{
        "recordName":"current-master","zoneID":{"zoneName":"SharedSync","ownerRecordName":"_defaultOwner"}
    }});
    PhotoAsset::new(pair["records"][0].clone(), pair["records"][1].clone())
        .with_source_zone(Arc::from("SharedSync"))
}

async fn run_current(
    configured: &Arc<DownloadConfig>,
    assets: Vec<PhotoAsset>,
    mode: DownloadRunMode,
) -> StreamingResult {
    assert!(
        !assets.is_empty(),
        "quiet rediscovery must enumerate current source"
    );
    let count = assets.len() as u64;
    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(assets.into_iter().map(Ok::<_, anyhow::Error>)),
        configured,
        DownloadControls::new(mode, DownloadReporting::hidden()),
        count,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();
    assert!(result.enumeration_complete, "{result:?}");
    eprintln!("BASELINE_A_CYCLE nonempty={count} {mode:?} {result:?}");
    result
}

async fn request_counts(server: &MockServer) -> (usize, usize) {
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|request| request.method == "GET"));
    let counts = (
        requests
            .iter()
            .filter(|request| request.url.path() == "/still")
            .count(),
        requests
            .iter()
            .filter(|request| request.url.path() == "/motion")
            .count(),
    );
    assert_eq!(requests.len(), counts.0 + counts.1);
    eprintln!("BASELINE_A_GET still={} motion={}", counts.0, counts.1);
    counts
}

fn assert_current_receipt(
    db_path: &Path,
    configured: &DownloadConfig,
    version: VersionSizeKey,
    media: &Path,
) -> Vec<Value> {
    let (source, tag) = match version {
        VersionSizeKey::Original => (STILL, 1),
        VersionSizeKey::LiveOriginal => (MOTION, 2),
        _ => panic!("fixture has only original still and motion"),
    };
    let (capture, scope, zone) = configured
        .primary_layout_source
        .as_ref()
        .unwrap()
        .publication_scope("SharedSync")
        .unwrap()
        .unwrap();
    let (account, provider) = capture.owner.authenticated_keys().unwrap();
    assert_eq!(
        zone,
        json!({"zoneName":"SharedSync","ownerRecordName":"_defaultOwner"})
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&scope).unwrap()["database"],
        "private"
    );
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(source));
    let native = crate::state::db::provider_selection::SelectionPath::from_path(media)
        .key()
        .unwrap();
    let conn = Connection::open(db_path).unwrap();
    let actual: Vec<Value> = conn.prepare("SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum FROM download_receipts WHERE library='SharedSync' AND child='current-child' AND version_size=?1").unwrap()
        .query_row([version.as_str()], |row| (0..14).map(|column| row.get(column)).collect()).unwrap();
    assert_eq!(
        actual,
        vec![
            account.to_owned().into(),
            provider.to_owned().into(),
            scope.clone().into(),
            "SharedSync".to_owned().into(),
            "current-child".to_owned().into(),
            "current-master".to_owned().into(),
            "current-child".to_owned().into(),
            configured
                .primary_layout_pass
                .as_ref()
                .unwrap()
                .to_string()
                .into(),
            version.as_str().to_owned().into(),
            generation(tag).into(),
            (source.len() as i64).into(),
            native.into(),
            digest.clone().into(),
            digest.clone().into(),
        ]
    );
    eprintln!(
        "BASELINE_A_RECEIPT {}",
        json!({"scope":scope,"pass":configured.primary_layout_pass,"child":"current-child","master":"current-master","version":version.as_str(),"native":media,"source_sha256":digest,"local_sha256":digest})
    );
    actual
}

#[tokio::test]
async fn baseline_stream_reuses_mixed_label_receipts_and_stable_paths_across_reopen() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 2, 0).await;
    for name in ["IMG_0001.JPG", "renamed-legacy-suffixed.JPG"] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("photos");
        let foreign = root.join("IMG_0001.JPG");
        let db_path = directory.path().join("state.db");
        let (db, configured) = open_current_pass(directory.path()).await;
        seed(&db_path, &foreign).await;
        let before = old_rows(&db_path);
        let foreign_time = std::fs::metadata(&foreign).unwrap().modified().unwrap();
        let setup_before = request_counts(&server).await;
        let preview = run_current(
            &configured,
            vec![named_current_asset(&server, name, false)],
            DownloadRunMode::PrintFilenames,
        )
        .await;
        assert_eq!(preview.printed_filenames.len(), 1, "{name}: {preview:?}");
        let media = preview.printed_filenames[0].clone();
        assert_ne!(
            media, foreign,
            "foreign occupied requested path cannot be aliased"
        );
        assert!(!media.exists());
        assert!(
            current_rows(&db_path).is_empty(),
            "preview cannot admit work"
        );
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(std::fs::read(&foreign).unwrap(), STILL);
        assert_eq!(
            std::fs::metadata(&foreign).unwrap().modified().unwrap(),
            foreign_time
        );
        assert_eq!(request_counts(&server).await, setup_before);
        let initial = run_current(
            &configured,
            vec![named_current_asset(&server, name, false)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(initial.downloaded, 1, "{initial:?}");
        assert!(initial.failed.is_empty());
        assert_eq!(initial.state_write_failures, 0);
        assert_eq!(request_counts(&server).await, (setup_before.0 + 1, 0));
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(std::fs::read(&foreign).unwrap(), STILL);
        assert_eq!(std::fs::read(&media).unwrap(), STILL);
        let receipt =
            assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media);
        let current_before = current_rows(&db_path);
        let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
        drop(configured);
        assert_eq!(
            Arc::strong_count(&db),
            1,
            "capture must close before reopen"
        );
        drop(db);

        for cycle in 0..3 {
            let (db, configured) = open_current_pass(directory.path()).await;
            let preview = run_current(
                &configured,
                vec![named_current_asset(&server, "IMG_0001.JPG", false)],
                DownloadRunMode::PrintFilenames,
            )
            .await;
            assert!(preview.printed_filenames.is_empty(), "{preview:?}");
            assert_eq!(current_rows(&db_path), current_before);
            assert_eq!(
                assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media),
                receipt
            );
            let result = run_current(
                &configured,
                vec![named_current_asset(&server, "IMG_0001.JPG", false)],
                DownloadRunMode::Download,
            )
            .await;
            assert_eq!(result.downloaded, 0, "cycle {cycle}: {result:?}");
            assert_eq!(result.state_write_failures, 0, "cycle {cycle}: {result:?}");
            assert!(result.failed.is_empty());
            assert_eq!(result.skip_summary.on_disk, 1, "cycle {cycle}: {result:?}");
            assert_eq!(
                current_rows(&db_path),
                vec![(
                    "original".into(),
                    "downloaded".into(),
                    media.to_string_lossy().into_owned()
                )]
            );
            assert_eq!(request_counts(&server).await, (setup_before.0 + 1, 0));
            drop(configured);
            assert_eq!(Arc::strong_count(&db), 1);
            drop(db);
            assert_eq!(
                old_rows(&db_path),
                before,
                "old receipt changed in cycle {cycle}"
            );
            assert_eq!(std::fs::read(&media).unwrap(), STILL);
            assert_eq!(
                std::fs::metadata(&media).unwrap().modified().unwrap(),
                mtime
            );
            assert_eq!(std::fs::read(&foreign).unwrap(), STILL);
            assert_eq!(
                std::fs::metadata(&foreign).unwrap().modified().unwrap(),
                foreign_time
            );
        }
        let files = std::fs::read_dir(&root)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            files.len(),
            2,
            "foreign and current files only; no duplicate or staging file"
        );
    }
    assert_eq!(request_counts(&server).await, (2, 0));
}

#[tokio::test]
async fn baseline_stream_wrong_same_size_bytes_stay_pending_without_transfer() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 1, 0).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("photos");
    let media = root.join("IMG_0001.JPG");
    let db_path = directory.path().join("state.db");
    let (db, configured) = open_current_pass(directory.path()).await;
    seed(&db_path, &root.join("foreign-primary.JPG")).await;
    let initial = run_current(
        &configured,
        vec![named_current_asset(&server, "IMG_0001.JPG", false)],
        DownloadRunMode::Download,
    )
    .await;
    assert_eq!(initial.downloaded, 1, "{initial:?}");
    assert!(initial.failed.is_empty());
    assert_eq!(initial.state_write_failures, 0);
    let receipt = assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media);
    assert_eq!(request_counts(&server).await, (1, 0));
    drop(configured);
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let mut changed = STILL.to_vec();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    std::fs::write(&media, &changed).unwrap();
    let before = old_rows(&db_path);
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();

    for cycle in 0..3 {
        let (db, configured) = open_current_pass(directory.path()).await;
        let preview = run_current(
            &configured,
            vec![named_current_asset(&server, "IMG_0001.JPG", false)],
            DownloadRunMode::PrintFilenames,
        )
        .await;
        assert!(preview.printed_filenames.is_empty());
        let result = run_current(
            &configured,
            vec![named_current_asset(&server, "IMG_0001.JPG", false)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(result.downloaded, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.skip_summary.on_disk, 0);
        assert_eq!(result.state_write_failures, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.skip_summary.by_state, 0);
        assert_eq!(result.skip_summary.uncertain, 1);
        let (outcome, stats) =
            crate::download::pipeline::test_support::build_zero_download_outcome(
                result,
                crate::download::DownloadControls::download_hidden(),
            )
            .await;
        assert!(matches!(
            outcome,
            crate::download::DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(stats.skipped.uncertain, 1);
        assert!(
            !stats.sync_token_blocked,
            "durable pending work does not veto observed source progress"
        );
        let rows = current_rows(&db_path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "pending");
        let conn = Connection::open(&db_path).unwrap();
        let verification: (String,String) = conn.query_row("SELECT state,reason FROM asset_verifications WHERE library='SharedSync' AND id='current-child' AND version_size='original'", [], |row|Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(
            verification,
            ("unknown".into(), "baseline_match_uncertain".into())
        );
        let diagnostics = db.pending_match_status(10).await.unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .attempt
                .explanation
                .candidates
                .iter()
                .any(|entry| entry
                    .path
                    .as_ref()
                    .is_some_and(|path| path.to_path() == media))
        );
        assert!(diagnostics[0].attempt.observed_at > 0);
        assert!(
            diagnostics[0].schedule.is_none(),
            "direct scoped pipeline must not invent a selection-ledger retry"
        );
        assert_eq!(
            assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media),
            receipt
        );
        assert_eq!(request_counts(&server).await, (1, 0));
        drop(conn);
        drop(configured);
        assert_eq!(Arc::strong_count(&db), 1);
        drop(db);
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(std::fs::read(&media).unwrap(), changed);
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            mtime
        );
    }
    assert_eq!(request_counts(&server).await, (1, 0));
}

#[tokio::test]
async fn baseline_stream_downloads_only_missing_companion_with_preview_parity() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 1, 1).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("photos");
    let media = root.join("IMG_0001.JPG");
    let db_path = directory.path().join("state.db");
    let (db, configured) = open_current_pass(directory.path()).await;
    seed(&db_path, &root.join("foreign-primary.JPG")).await;
    let initial = run_current(
        &configured,
        vec![named_current_asset(&server, "IMG_0001.JPG", false)],
        DownloadRunMode::Download,
    )
    .await;
    assert_eq!(initial.downloaded, 1, "{initial:?}");
    assert!(initial.failed.is_empty());
    assert_eq!(initial.state_write_failures, 0);
    let receipt = assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media);
    assert_eq!(request_counts(&server).await, (1, 0));
    drop(configured);
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let before = old_rows(&db_path);
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
    let (db, configured) = open_current_pass(directory.path()).await;
    let preview = run_current(
        &configured,
        vec![named_current_asset(&server, "IMG_0001.JPG", true)],
        DownloadRunMode::PrintFilenames,
    )
    .await;
    assert_eq!(preview.printed_filenames.len(), 1, "{preview:?}");
    let destination = preview.printed_filenames[0].clone();
    assert!(!destination.exists());
    assert_eq!(request_counts(&server).await, (1, 0));
    let result = run_current(
        &configured,
        vec![named_current_asset(&server, "IMG_0001.JPG", true)],
        DownloadRunMode::Download,
    )
    .await;
    assert_eq!(result.downloaded, 1, "{result:?}");
    assert!(result.failed.is_empty(), "{result:?}");
    assert_eq!(result.state_write_failures, 0, "{result:?}");
    assert_eq!(std::fs::read(&destination).unwrap(), MOTION);
    assert_eq!(std::fs::read(&media).unwrap(), STILL);
    assert_eq!(
        std::fs::metadata(&media).unwrap().modified().unwrap(),
        mtime
    );
    assert_eq!(old_rows(&db_path), before);
    let rows = current_rows(&db_path);
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row.0 == VersionSizeKey::LiveOriginal.as_str()
                && row.2 == destination.to_string_lossy())
    );
    let motion_receipt = assert_current_receipt(
        &db_path,
        &configured,
        VersionSizeKey::LiveOriginal,
        &destination,
    );
    assert_eq!(request_counts(&server).await, (1, 1));
    drop(configured);
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);

    for cycle in 1..3 {
        let (db, configured) = open_current_pass(directory.path()).await;
        let preview = run_current(
            &configured,
            vec![named_current_asset(&server, "IMG_0001.JPG", true)],
            DownloadRunMode::PrintFilenames,
        )
        .await;
        assert!(
            preview.printed_filenames.is_empty(),
            "cycle {cycle}: {preview:?}"
        );
        let repeated = run_current(
            &configured,
            vec![named_current_asset(&server, "IMG_0001.JPG", true)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(repeated.downloaded, 0, "cycle {cycle}: {repeated:?}");
        assert_eq!(
            repeated.state_write_failures, 0,
            "cycle {cycle}: {repeated:?}"
        );
        assert_eq!(current_rows(&db_path), rows);
        assert_eq!(
            assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media),
            receipt
        );
        assert_eq!(
            assert_current_receipt(
                &db_path,
                &configured,
                VersionSizeKey::LiveOriginal,
                &destination
            ),
            motion_receipt
        );
        assert_eq!(request_counts(&server).await, (1, 1));
        drop(configured);
        assert_eq!(Arc::strong_count(&db), 1);
        drop(db);
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(std::fs::read(&media).unwrap(), STILL);
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            mtime
        );
    }
    assert_eq!(request_counts(&server).await, (1, 1));
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn baseline_stream_enabled_metadata_writers_preserve_existing_media_and_sidecar() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 1, 0).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("photos");
    let media = root.join("renamed-old.JPG");
    let sidecar = root.join("renamed-old.JPG.xmp");
    let db_path = directory.path().join("state.db");
    let (db, configured) = open_current_pass(directory.path()).await;
    seed(&db_path, &root.join("foreign-primary.JPG")).await;
    let initial = run_current(
        &configured,
        vec![named_current_asset(&server, "renamed-old.JPG", false)],
        DownloadRunMode::Download,
    )
    .await;
    assert_eq!(initial.downloaded, 1, "{initial:?}");
    assert!(initial.failed.is_empty());
    assert_eq!(initial.state_write_failures, 0);
    let receipt = assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media);
    assert_eq!(request_counts(&server).await, (1, 0));
    drop(configured);
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    std::fs::write(&sidecar, b"existing sidecar metadata").unwrap();
    let media_time = std::fs::metadata(&media).unwrap().modified().unwrap();
    let sidecar_time = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
    let before = old_rows(&db_path);
    for cycle in 0..2 {
        let (db, configured) = open_current_pass(directory.path()).await;
        let mut configured = Arc::unwrap_or_clone(configured);
        configured.metadata.embed_xmp = true;
        configured.metadata.xmp_sidecar = true;
        configured.metadata.set_exif_datetime = true;
        let configured = Arc::new(configured);
        assert!(!configured.refresh_metadata);
        let result = run_current(
            &configured,
            vec![named_current_asset(&server, "IMG_0001.JPG", false)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(result.downloaded, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.state_write_failures, 0, "cycle {cycle}: {result:?}");
        assert_eq!(std::fs::read(&media).unwrap(), STILL);
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"existing sidecar metadata"
        );
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            media_time
        );
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
            sidecar_time
        );
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(
            assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media),
            receipt
        );
        assert_eq!(current_rows(&db_path)[0].2, media.to_string_lossy());
        assert_eq!(request_counts(&server).await, (1, 0));
        drop(configured);
        assert_eq!(Arc::strong_count(&db), 1);
        drop(db);
    }
    assert_eq!(request_counts(&server).await, (1, 0));
}

mod album_tests;

mod provider_signature;

#[tokio::test]
async fn baseline_stream_uncertain_still_and_downloaded_motion_reports_partial() {
    let server = crate::start_wiremock_or_skip!();
    mount(&server, 1, 1).await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("photos");
    let media = root.join("IMG_0001.JPG");
    let db_path = directory.path().join("state.db");
    let (db, configured) = open_current_pass(directory.path()).await;
    seed(&db_path, &root.join("foreign-primary.JPG")).await;
    let initial = run_current(
        &configured,
        vec![named_current_asset(&server, "IMG_0001.JPG", false)],
        DownloadRunMode::Download,
    )
    .await;
    assert_eq!(initial.downloaded, 1, "{initial:?}");
    assert!(initial.failed.is_empty());
    assert_eq!(initial.state_write_failures, 0);
    let receipt = assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media);
    assert_eq!(request_counts(&server).await, (1, 0));
    drop(configured);
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let mut changed = STILL.to_vec();
    *changed.last_mut().unwrap() ^= 1;
    std::fs::write(&media, &changed).unwrap();
    let before = old_rows(&db_path);
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
    for cycle in 0..3 {
        let (db, configured) = open_current_pass(directory.path()).await;
        let preview = run_current(
            &configured,
            vec![named_current_asset(&server, "IMG_0001.JPG", true)],
            DownloadRunMode::PrintFilenames,
        )
        .await;
        assert_eq!(preview.printed_filenames.len(), usize::from(cycle == 0));
        assert_eq!(old_rows(&db_path), before);
        let result = run_current(
            &configured,
            vec![named_current_asset(&server, "IMG_0001.JPG", true)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(result.downloaded, usize::from(cycle == 0));
        assert!(result.failed.is_empty());
        assert_eq!(result.state_write_failures, 0);
        assert_eq!(
            result.skip_summary.uncertain, 1,
            "cycle {cycle}: {result:?}"
        );
        // Despite its historical name this helper passes the complete result to
        // build_download_outcome; no failed tasks means it performs no retry I/O.
        let (outcome, stats) =
            crate::download::pipeline::test_support::build_zero_download_outcome(
                result,
                DownloadControls::download_hidden(),
            )
            .await;
        assert!(matches!(
            outcome,
            crate::download::DownloadOutcome::PartialFailure { failed_count: 1 }
        ));
        assert_eq!(stats.skipped.uncertain, 1);
        assert!(!stats.sync_token_blocked);
        let rows = current_rows(&db_path);
        assert!(
            rows.iter()
                .any(|row| row.0 == "original" && row.1 == "pending")
        );
        assert!(
            rows.iter()
                .any(|row| row.0 == VersionSizeKey::LiveOriginal.as_str() && row.1 == "downloaded")
        );
        assert_eq!(std::fs::read(&media).unwrap(), changed);
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            mtime
        );
        assert_eq!(old_rows(&db_path), before);
        assert_eq!(
            assert_current_receipt(&db_path, &configured, VersionSizeKey::Original, &media),
            receipt
        );
        let motion = rows
            .iter()
            .find(|row| row.0 == VersionSizeKey::LiveOriginal.as_str())
            .unwrap();
        assert_eq!(std::fs::read(&motion.2).unwrap(), MOTION);
        assert_current_receipt(
            &db_path,
            &configured,
            VersionSizeKey::LiveOriginal,
            Path::new(&motion.2),
        );
        assert_eq!(request_counts(&server).await, (1, 1));
        drop(configured);
        assert_eq!(Arc::strong_count(&db), 1);
        drop(db);
    }
    assert_eq!(request_counts(&server).await, (1, 1));
}
