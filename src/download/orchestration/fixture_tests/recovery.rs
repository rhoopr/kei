use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::commands::{ImportRunOptions, import_assets};
use crate::config::MediaSelection;
use crate::download::DownloadOutcome;
use crate::download::paths::DirCache;

use super::support::Harness;
use super::{cycle, fixture, pass};

#[tokio::test]
async fn bundled_import_media_filter_roundtrip() {
    for policy in [
        crate::types::FileMatchPolicy::NameSizeDedupWithSuffix,
        crate::types::FileMatchPolicy::NameId7,
    ] {
        let mut h = Harness::new().await;
        h.config.file_match_policy = policy;
        let filename = if policy == crate::types::FileMatchPolicy::NameId7 {
            "Photo_cGhvdG8.JPG"
        } else {
            "Photo.JPG"
        };
        h.config.media = MediaSelection {
            photos: true,
            videos: false,
            live_photos: false,
        };
        let mut records = h
            .asset("photo", "Photo.jpg", "public.jpeg", "media/pattern.jpg", 0)
            .await;
        records.extend(
            h.asset(
                "video",
                "Video.MOV",
                "com.apple.quicktime-movie",
                "media/pattern.mov",
                0,
            )
            .await,
        );
        std::fs::create_dir_all(&h.config.directory).unwrap();
        std::fs::write(
            h.config.directory.join(filename),
            fixture("media/pattern.jpg"),
        )
        .unwrap();
        // Excluded local video remains user-owned, never adopted or modified.
        let video = h.config.directory.join("Video.MOV");
        std::fs::write(&video, fixture("media/pattern.mov")).unwrap();
        let album = pass(records.clone()).album;
        let (stream, panic_rx) = album.photo_stream(None, None, 1);
        let stats = import_assets(
            stream,
            panic_rx,
            h.db(),
            &h.config,
            "PrimarySync",
            &mut DirCache::new(),
            ImportRunOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.matched, 1);
        assert_eq!(stats.filtered, 1);
        h.reopen().await;
        for _ in 0..2 {
            let result = cycle(&h.config, records.clone()).await;
            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "{result:?}"
            );
            assert_eq!(result.stats.downloaded, 0);
            let rows = h.db().get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].local_path.as_ref(),
                Some(&h.config.directory.join(filename))
            );
            assert_eq!(
                std::fs::read(h.config.directory.join(filename)).unwrap(),
                fixture("media/pattern.jpg")
            );
            assert_eq!(std::fs::read(&video).unwrap(), fixture("media/pattern.mov"));
        }
        h.server.verify().await;
    }
}

#[tokio::test]
async fn bundled_import_recent_caps_each_pass() {
    let h = Harness::new().await;
    let mut records = Vec::new();
    for id in ["one", "two", "three"] {
        records.extend(
            h.asset(
                id,
                &format!("{id}.jpg"),
                "public.jpeg",
                "media/pattern.jpg",
                0,
            )
            .await,
        );
    }
    // Same bounded stream entry point as run_import_existing, over two passes.
    for _ in 0..2 {
        let album = pass(records.clone()).album;
        let (stream, panic_rx) = album.photo_stream(Some(2), None, 1);
        let stats = import_assets(
            stream,
            panic_rx,
            h.db(),
            &h.config,
            "PrimarySync",
            &mut DirCache::new(),
            ImportRunOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.unmatched, 2);
        assert_eq!(stats.matched, 0);
    }
    assert!(h.db().get_downloaded_page(0, 10).await.unwrap().is_empty());
    h.server.verify().await;
}

#[tokio::test]
async fn bundled_partial_failure_recovers_after_restart() {
    let mut h = Harness::new().await;
    h.config.concurrent_downloads = 1;
    let mut records = h
        .asset("good", "Good.jpg", "public.jpeg", "media/pattern.jpg", 1)
        .await;
    let mut bad = h
        .asset("bad", "Retry.jpg", "public.jpeg", "media/metadata.jpg", 0)
        .await;
    bad[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        serde_json::json!(format!("{}/failure", h.server.uri()));
    Mock::given(method("GET"))
        .and(path("/failure"))
        .respond_with(ResponseTemplate::new(503))
        .expect(2)
        .mount(&h.server)
        .await;
    records.extend(bad);
    assert!(h.db().get_downloaded_page(0, 10).await.unwrap().is_empty());
    let result = cycle(&h.config, records.clone()).await;
    assert!(
        matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { failed_count: 1 }
        ),
        "{result:?}"
    );
    assert_eq!(result.stats.downloaded, 1);
    h.assert_files(&[("Good.JPG", "media/pattern.jpg")]).await;
    let good = h.config.directory.join("Good.JPG");
    let modified = std::fs::metadata(&good).unwrap().modified().unwrap();
    h.reopen().await;
    assert_eq!(h.db().get_failed().await.unwrap().len(), 1);
    h.server.verify().await;
    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/failure"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture("media/metadata.jpg")))
        .expect(1)
        .mount(&h.server)
        .await;
    for downloaded in [1, 0] {
        let result = cycle(&h.config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, downloaded);
        h.assert_files(&[
            ("Good.JPG", "media/pattern.jpg"),
            ("Retry.JPG", "media/metadata.jpg"),
        ])
        .await;
        assert_eq!(
            std::fs::metadata(&good).unwrap().modified().unwrap(),
            modified
        );
        assert!(h.db().get_failed().await.unwrap().is_empty());
        assert!(h.db().get_pending().await.unwrap().is_empty());
        h.reopen().await;
    }
    h.server.verify().await;
}

#[tokio::test]
async fn bundled_no_overwrite_preserves_existing_media() {
    let mut h = Harness::new().await;
    let records = h
        .asset(
            "collision",
            "Photo.jpg",
            "public.jpeg",
            "media/pattern.jpg",
            1,
        )
        .await;
    std::fs::create_dir_all(&h.config.directory).unwrap();
    let existing = h.config.directory.join("Photo.JPG");
    let existing_bytes = fixture("media/metadata.jpg");
    std::fs::write(&existing, &existing_bytes).unwrap();
    let modified = std::fs::metadata(&existing).unwrap().modified().unwrap();
    let source = fixture("media/pattern.jpg");
    let expected = h
        .config
        .directory
        .join(format!("Photo-{}.JPG", source.len()));
    for downloaded in [1, 0] {
        let result = cycle(&h.config, records.clone()).await;
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        assert_eq!(result.stats.downloaded, downloaded);
        assert!(std::fs::read(&existing).unwrap() == existing_bytes);
        assert_eq!(
            std::fs::metadata(&existing).unwrap().modified().unwrap(),
            modified
        );
        assert!(std::fs::read(&expected).unwrap() == source);
        h.reopen().await;
        let rows = h.db().get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].local_path.as_ref(), Some(&expected));
        assert_eq!(super::support::files(&h.config.directory).len(), 2);
    }
    h.server.verify().await;
}

#[tokio::test]
async fn bundled_interruption_retries_after_restart() {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    let mut h = Harness::new().await;
    let mut records = h
        .asset(
            "interruption",
            "Photo.jpg",
            "public.jpeg",
            "media/pattern.jpg",
            0,
        )
        .await;
    records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        serde_json::json!(format!("{}/delayed", h.server.uri()));
    Mock::given(method("GET"))
        .and(path("/delayed"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(fixture("media/pattern.jpg"))
                .set_delay(Duration::from_secs(1)),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    let token = CancellationToken::new();
    let config = Arc::new(h.config.clone());
    let passes = vec![pass(records.clone())];
    let child_token = token.clone();
    let worker = async move {
        super::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes,
            config,
            crate::download::DownloadControls::download_hidden(),
            child_token,
        )
        .await
        .unwrap()
    };
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if h.server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path() == "/delayed")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("download entered HTTP boundary");
        token.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(Box::pin(worker), cancel)
    })
    .await
    .unwrap();
    assert!(result.stats.interrupted);
    assert_eq!(result.stats.downloaded, 0);
    assert!(!h.config.directory.join("Photo.JPG").exists());
    h.reopen().await;
    assert!(h.db().get_downloaded_page(0, 10).await.unwrap().is_empty());
    assert_eq!(
        h.db().get_pending().await.unwrap().len() + h.db().get_failed().await.unwrap().len(),
        1
    );
    h.server.verify().await;
    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/delayed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture("media/pattern.jpg")))
        .expect(1)
        .mount(&h.server)
        .await;
    h.stable(records, &[("Photo.JPG", "media/pattern.jpg")])
        .await;
}
