//! Bundled media reaches provider decoding, planning, HTTP, publication and SQLite.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::download::{DownloadConfig, DownloadControls, DownloadOutcome};
use crate::state::SqliteStateDb;
use crate::test_helpers::MockPhotosFlow;

use super::dispatch::download_photos_with_sync;
use super::models::SyncMode;
use super::test_support::{incremental_photo_records_with_url, mock_album};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name),
    )
    .expect("bundled fixture must exist in the source package")
}

fn sha256(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn records(name: &str, uti: &str, server: &MockServer, bytes: &[u8]) -> Vec<Value> {
    let mut records = incremental_photo_records_with_url(
        "fixture-master",
        name,
        &format!("{}/media", server.uri()),
        bytes.len() as u64,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)));
    records[0]["fields"]["itemType"] = json!({"value": uti});
    records[0]["fields"]["resOriginalFileType"] = json!({"value": uti});
    records[1]["fields"]["isFavorite"] = json!({"value": 1});
    records
}

fn pass(records: Vec<Value>) -> AlbumPass {
    AlbumPass {
        kind: PassKind::Unfiled,
        album: mock_album(
            "",
            MockPhotosFlow::new()
                .album_count(1)
                .query_page(records, Some("fixture-token"))
                .empty_query_page(Some("fixture-token"))
                .build(),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }
}

async fn cycle(config: &DownloadConfig, records: Vec<Value>) -> super::models::SyncResult {
    download_photos_with_sync(
        &Client::new(),
        &[pass(records)],
        Arc::new(config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("fixture production cycle")
}

#[tokio::test]
async fn bundled_media_download_finalize_reopen_and_second_sync() {
    for (name, uti, mime) in [
        ("media/pattern.jpg", "public.jpeg", "image/jpeg"),
        ("media/metadata.jpg", "public.jpeg", "image/jpeg"),
        ("media/pattern.png", "public.png", "image/png"),
        ("media/pattern.heic", "public.heic", "image/heic"),
        ("media/pattern.avif", "public.avif", "image/avif"),
        (
            "media/pattern.mov",
            "com.apple.quicktime-movie",
            "video/quicktime",
        ),
        ("media/pattern.mp4", "public.mpeg-4", "video/mp4"),
        (
            "media/pattern.dng",
            "com.adobe.raw-image",
            "image/x-adobe-dng",
        ),
        ("media/apple-live.heic", "public.heic", "image/heic"),
        (
            "media/apple-live.mov",
            "com.apple.quicktime-movie",
            "video/quicktime",
        ),
        ("apple-hdr-gainmap.heic", "public.heic", "image/heic"),
        ("white_1x1.avif", "public.avif", "image/avif"),
    ] {
        let source = fixture(name);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/media"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(source.clone())
                    .insert_header("content-type", mime),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = String::new();
        config.sync_mode = SyncMode::Full;
        config.state_db = Some(db.clone());
        let filename = Path::new(name).file_name().unwrap().to_str().unwrap();
        let records = records(filename, uti, &server, &source);
        let result = cycle(&config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{name}: {result:?}"
        );
        assert_eq!(result.stats.downloaded, 1, "{name}");
        let row = db.get_downloaded_page(0, 10).await.unwrap().remove(0);
        let final_path = row.local_path.unwrap();
        assert!(
            std::fs::read(&final_path).unwrap() == source,
            "{name}: bytes changed"
        );
        assert_eq!(
            row.download_checksum.as_deref(),
            Some(sha256(&source).as_str())
        );
        assert_eq!(
            row.local_checksum.as_deref(),
            Some(sha256(&source).as_str())
        );
        config.state_db = None;
        drop(db);
        let reopened = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        config.state_db = Some(reopened.clone());
        let result = cycle(&config, records).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{name}: {result:?}"
        );
        assert_eq!(result.stats.downloaded, 0, "{name}");
        assert_eq!(reopened.get_downloaded_page(0, 10).await.unwrap().len(), 1);
        assert!(reopened.get_pending().await.unwrap().is_empty());
        assert!(reopened.get_failed().await.unwrap().is_empty());
        assert!(
            std::fs::read(&final_path).unwrap() == source,
            "{name}: bytes changed"
        );
        assert_eq!(
            std::fs::read_dir(&config.directory).unwrap().count(),
            1,
            "only the final media file may remain, without part files or sidecars"
        );
        server.verify().await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn bundled_heif_prepublication_xmp_preserves_media_and_checksum_roles() {
    use crate::download::heif;
    use xmp_toolkit::{XmpMeta, xmp_ns};

    for name in [
        "media/apple-live.heic",
        "media/pattern.heic",
        "apple-hdr-gainmap.heic",
        "media/pattern.avif",
    ] {
        let source = fixture(name);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/media"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(source.clone())
                    .insert_header("content-type", "image/heic"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = String::new();
        config.sync_mode = SyncMode::Full;
        config.metadata.embed_xmp = true;
        config.metadata.set_exif_rating = true;
        config.state_db = Some(db.clone());
        let filename = Path::new(name).file_name().unwrap().to_str().unwrap();
        let uti = if filename.ends_with("avif") {
            "public.avif"
        } else {
            "public.heic"
        };
        let records = records(filename, uti, &server, &source);
        let result = cycle(&config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{name}: {result:?}"
        );
        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.stats.exif_failures, 0);
        let row = db.get_downloaded_page(0, 10).await.unwrap().remove(0);
        let path = row.local_path.unwrap();
        let written = std::fs::read(&path).unwrap();
        assert!(
            source != written,
            "{name}: metadata must change the downloaded bytes"
        );
        // Optional audit output lets an independent decoder compare the real
        // pipeline result with the source. Normal tests need no external tools.
        if let Some(root) = std::env::var_os("KEI_FIXTURE_INSPECT_DIR") {
            let output = Path::new(&root).join(name);
            std::fs::create_dir_all(output.parent().unwrap()).unwrap();
            std::fs::write(output, &written).unwrap();
        }
        heif::validate_rewrite_preserves_non_xmp_items(&source, &written).unwrap();
        let packet = heif::extract_xmp_bytes(&written).unwrap();
        let xmp: XmpMeta = std::str::from_utf8(&packet).unwrap().parse().unwrap();
        assert_eq!(xmp.property(xmp_ns::XMP, "Rating").unwrap().value, "5");
        assert_eq!(
            row.download_checksum.as_deref(),
            Some(sha256(&source).as_str())
        );
        assert_eq!(
            row.local_checksum.as_deref(),
            Some(sha256(&written).as_str())
        );
        config.state_db = None;
        drop(db);
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        config.state_db = Some(db.clone());
        let result = cycle(&config, records).await;
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 0);
        assert!(
            std::fs::read(&path).unwrap() == written,
            "{name}: steady state rewrote media"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(std::fs::read_dir(&config.directory).unwrap().count(), 1);
        server.verify().await;
    }
}

#[tokio::test]
async fn bundled_live_photo_modes_preserve_pair_and_companion_naming() {
    use crate::types::LivePhotoMode;

    let still = fixture("media/apple-live.heic");
    let movie = fixture("media/apple-live.mov");
    for (mode, expected_names) in [
        (LivePhotoMode::Both, vec!["Live.HEIC", "Live_HEVC.MOV"]),
        (LivePhotoMode::ImageOnly, vec!["Live.HEIC"]),
        (LivePhotoMode::VideoOnly, vec!["Live_HEVC.MOV"]),
        (LivePhotoMode::Skip, vec![]),
    ] {
        let server = MockServer::start().await;
        for (endpoint, bytes, mime, wanted) in [
            (
                "/media",
                &still,
                "image/heic",
                expected_names.contains(&"Live.HEIC"),
            ),
            (
                "/movie",
                &movie,
                "video/quicktime",
                expected_names.contains(&"Live_HEVC.MOV"),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(endpoint))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(bytes.clone())
                        .insert_header("content-type", mime),
                )
                .expect(u64::from(wanted))
                .mount(&server)
                .await;
        }
        let mut records = records("Live.HEIC", "public.heic", &server, &still);
        records[0]["fields"]["resOriginalVidComplRes"] = json!({"value": {
            "downloadURL": format!("{}/movie", server.uri()),
            "size": movie.len(), "fileChecksum": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&movie)),
        }});
        records[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value": "com.apple.quicktime-movie"});
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = String::new();
        config.sync_mode = SyncMode::Full;
        config.live_photo_mode = mode;
        config.state_db = Some(db.clone());
        let result = cycle(&config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{mode:?}: {:?}; failed={:?}",
            result.outcome,
            db.get_failed().await.unwrap()
        );
        assert_eq!(result.stats.downloaded, expected_names.len(), "{mode:?}");
        config.state_db = None;
        drop(db);
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        config.state_db = Some(db.clone());
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        let mut names: Vec<_> = rows
            .iter()
            .map(|row| {
                let final_path = row.local_path.as_ref().unwrap();
                let bytes = if final_path.extension().unwrap() == "HEIC" {
                    &still
                } else {
                    &movie
                };
                assert!(&std::fs::read(final_path).unwrap() == bytes);
                final_path.file_name().unwrap().to_str().unwrap().to_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, expected_names, "{mode:?}");
        let result = cycle(&config, records).await;
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.downloaded, 0);
        assert!(db.get_pending().await.unwrap().is_empty());
        assert!(db.get_failed().await.unwrap().is_empty());
        let file_count = if config.directory.exists() {
            std::fs::read_dir(&config.directory).unwrap().count()
        } else {
            0
        };
        assert_eq!(file_count, expected_names.len(), "{mode:?}: orphaned file");
        server.verify().await;
    }
}

#[tokio::test]
async fn bundled_invalid_download_retains_retry_evidence_then_recovers_after_restart() {
    let source = fixture("media/pattern.jpg");
    for bad in [
        vec![0_u8; source.len()],
        source[..source.len() / 2].to_vec(),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bad)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;
        let records = records("retry.jpg", "public.jpeg", &server, &source);
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = String::new();
        config.sync_mode = SyncMode::Full;
        config.retry.max_retries = 0;
        config.state_db = Some(db.clone());
        let result = cycle(&config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::PartialFailure { .. }),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        assert!(!config.directory.join("retry.jpg").exists());
        for entry in std::fs::read_dir(&config.directory).unwrap() {
            assert!(
                entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .ends_with(config.temp_suffix.as_ref()),
                "failed validation must not publish a final media file"
            );
        }
        config.state_db = None;
        drop(db);
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        assert_eq!(db.get_failed().await.unwrap().len(), 1);
        assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
        config.state_db = Some(db.clone());
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(source.clone())
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
        for expected_downloads in [1, 0] {
            let result = cycle(&config, records.clone()).await;
            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "{result:?}"
            );
            assert_eq!(result.stats.downloaded, expected_downloads);
            let row = db.get_downloaded_page(0, 10).await.unwrap().remove(0);
            assert!(std::fs::read(row.local_path.unwrap()).unwrap() == source);
            assert!(db.get_pending().await.unwrap().is_empty());
            assert!(db.get_failed().await.unwrap().is_empty());
            assert_eq!(std::fs::read_dir(&config.directory).unwrap().count(), 1);
        }
        server.verify().await;
    }
}
