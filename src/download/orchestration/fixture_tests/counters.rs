use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::commands::{AlbumPass, PassKind};
use crate::cycle_reporter::{CycleFacts, CycleReporter, CycleReporterConfig};
use crate::download::{DownloadOutcome, SyncStats};
use crate::health::HealthStatus;
use crate::icloud::photos::PhotosSession;
use crate::metrics::{MetricsHandle, render_metrics_for_test};
use crate::notifications::Notifier;
use crate::personality::Mode;
use crate::report::RunOptions;

use super::super::test_support::album_with_session;
use super::support::Harness;
use super::{cycle_passes, fixture, pass};

const EXPECTED_FILES: &[(&str, &str)] = &[
    ("Photos/First.JPG", "media/pattern.jpg"),
    ("Photos/Second.JPG", "media/metadata.jpg"),
    ("Movies/Movie.MOV", "media/pattern.mov"),
];

fn album_pass(name: &str, records: Vec<Value>) -> AlbumPass {
    let mut p = pass(records);
    p.kind = PassKind::Album;
    p.album.name = Arc::from(name);
    p
}

async fn assets(h: &Harness) -> (Vec<Value>, Vec<Value>) {
    let mut photos = h
        .asset("first", "First.jpg", "public.jpeg", "media/pattern.jpg", 1)
        .await;
    photos.extend(
        h.asset(
            "second",
            "Second.jpg",
            "public.jpeg",
            "media/metadata.jpg",
            1,
        )
        .await,
    );
    let movie = h
        .asset(
            "movie",
            "Movie.MOV",
            "com.apple.quicktime-movie",
            "media/pattern.mov",
            1,
        )
        .await;
    (photos, movie)
}

fn assert_transfer_stats(
    stats: &SyncStats,
    downloaded: usize,
    bytes: u64,
    photos: usize,
    videos: usize,
) {
    assert_eq!(stats.downloaded, downloaded);
    assert_eq!(stats.bytes_downloaded, bytes);
    assert_eq!(stats.disk_bytes_written, bytes);
    assert_eq!(stats.photos_downloaded, photos);
    assert_eq!(stats.videos_downloaded, videos);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.state_write_failures, 0);
}

async fn assert_reporting(
    h: &Harness,
    stats: &SyncStats,
    metrics: &MetricsHandle,
    total_bytes: u64,
) {
    // Feed the production final stats into the same reporting boundary used by sync/watch.
    let dir = h.config.directory.parent().unwrap();
    let report_path = dir.join("sync_report.json");
    let config = crate::config::Config::build(
        &crate::config::GlobalArgs {
            username: Some("counter-fixture@example.invalid".into()),
            domain: None,
            data_dir: Some(dir.to_string_lossy().into_owned()),
        },
        &crate::cli::PasswordArgs::default(),
        crate::cli::SyncArgs {
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(h.config.directory.to_string_lossy().into_owned()),
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let notifier = Notifier::new(None);
    let reporter = CycleReporter::new(CycleReporterConfig {
        watch_mode: false,
        report_path: Some(&report_path),
        run_options: RunOptions::from_config(&config),
        health_dir: dir,
        personality_mode: Mode::Off,
        state_db: Some(h.db()),
        metrics_handle: Some(metrics),
        notifier: &notifier,
    });
    reporter
        .report_completed_cycle(
            &mut HealthStatus::new(),
            CycleFacts::new(stats, 0, false, Duration::ZERO),
        )
        .await;
    let report: Value = serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
    assert_eq!(report["status"], "success");
    for field in [
        "downloaded",
        "bytes_downloaded",
        "disk_bytes_written",
        "photos_downloaded",
        "videos_downloaded",
    ] {
        assert_eq!(
            report["stats"][field],
            serde_json::to_value(stats).unwrap()[field]
        );
    }
    let rendered = render_metrics_for_test(metrics).await;
    for name in [
        "kei_sync_bytes_downloaded_total",
        "kei_sync_disk_bytes_written_total",
    ] {
        assert!(
            rendered
                .lines()
                .any(|line| line == format!("{name} {total_bytes}")),
            "{rendered}"
        );
    }
    assert!(
        rendered
            .lines()
            .any(|line| line == "kei_sync_downloaded_total 3"),
        "{rendered}"
    );
}

async fn assert_normal_passes(multiple_passes: bool) {
    let expected_bytes = [
        "media/pattern.jpg",
        "media/metadata.jpg",
        "media/pattern.mov",
    ]
    .iter()
    .map(|name| fixture(name).len() as u64)
    .sum::<u64>();
    assert_eq!(expected_bytes, 8_722);
    let mut h = Harness::new().await;
    h.config.folder_structure_albums = Arc::from("{album}");
    let (photos, movie) = assets(&h).await;
    assert!(!h.config.directory.exists());
    assert!(h.db().get_downloaded_page(0, 10).await.unwrap().is_empty());
    let make_passes = || {
        if multiple_passes {
            vec![
                album_pass("Photos", photos.clone()),
                album_pass("Movies", movie.clone()),
            ]
        } else {
            let mut all = photos.clone();
            all.extend(movie.clone());
            vec![album_pass("Photos", all)]
        }
    };
    let expected_files = if multiple_passes {
        EXPECTED_FILES.to_vec()
    } else {
        vec![
            EXPECTED_FILES[0],
            EXPECTED_FILES[1],
            ("Photos/Movie.MOV", "media/pattern.mov"),
        ]
    };
    let metrics = MetricsHandle::new(None);
    let result = cycle_passes(&h.config, &make_passes()).await;
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "{result:?}"
    );
    assert_transfer_stats(&result.stats, 3, expected_bytes, 2, 1);
    assert_eq!(result.stats.assets_seen, 3);
    assert_eq!(result.stats.same_cycle_recovery_attempts, 0);
    assert_reporting(&h, &result.stats, &metrics, expected_bytes).await;
    h.reopen().await;
    h.assert_files(&expected_files).await;
    let before = h.db().get_downloaded_page(0, 10).await.unwrap();
    let unchanged = cycle_passes(&h.config, &make_passes()).await;
    assert!(
        matches!(unchanged.outcome, DownloadOutcome::Success),
        "{unchanged:?}"
    );
    assert_transfer_stats(&unchanged.stats, 0, 0, 0, 0);
    assert_eq!(unchanged.stats.assets_seen, 3);
    assert_eq!(unchanged.stats.skipped.on_disk, 3);
    assert_reporting(&h, &unchanged.stats, &metrics, expected_bytes).await;
    h.reopen().await;
    h.assert_files(&expected_files).await;
    let after = h.db().get_downloaded_page(0, 10).await.unwrap();
    for row in &before {
        let current = after.iter().find(|other| other.id == row.id).unwrap();
        assert_eq!(current.downloaded_at, row.downloaded_at);
    }
    assert!(h.db().get_pending().await.unwrap().is_empty());
    assert!(h.db().get_failed().await.unwrap().is_empty());
    h.server.verify().await;
}

#[tokio::test]
async fn bundled_streaming_counters_one_pass_reports_metrics_and_restart() {
    assert_normal_passes(false).await;
}

#[tokio::test]
async fn bundled_streaming_counters_multiple_passes_reports_metrics_and_restart() {
    assert_normal_passes(true).await;
}

#[derive(Clone, Debug)]
struct CounterRecoverySession {
    calls: Arc<AtomicUsize>,
    initial: Vec<Value>,
    recovered: Vec<Value>,
}

#[async_trait::async_trait]
impl PhotosSession for CounterRecoverySession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({"batch": [{"records": [{"fields": {"itemCount": {"value": 3}}}]}]}));
        }
        assert!(url.contains("/records/query?"), "unexpected fixture route");
        // The initial pass has one data page plus the five empty stream/tail
        // probes, as in full_sync_repairs_missing_pass_token_in_same_cycle.
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let recovery = call >= 6;
        let records = if call.is_multiple_of(6) {
            if recovery {
                self.recovered.clone()
            } else {
                self.initial.clone()
            }
        } else {
            Vec::new()
        };
        let mut response = json!({"records": records});
        if recovery {
            response["syncToken"] = json!("fixture-token");
        }
        Ok(response)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn bundled_streaming_counters_include_token_recovery_transfers_without_recounting_inventory()
{
    let mut h = Harness::new().await;
    h.config.folder_structure_albums = Arc::from("{album}");
    h.config.concurrent_downloads = 1;
    let (mut all, movie) = assets(&h).await;
    all.extend(movie);
    let expected_bytes = [
        "media/pattern.jpg",
        "media/metadata.jpg",
        "media/pattern.mov",
    ]
    .iter()
    .map(|name| fixture(name).len() as u64)
    .sum::<u64>();
    assert_eq!(expected_bytes, 8_722);
    let calls = Arc::new(AtomicUsize::new(0));
    let session = CounterRecoverySession {
        calls: Arc::clone(&calls),
        initial: all[..2].to_vec(),
        recovered: all.clone(),
    };
    let mut recovery_pass = album_pass("Photos", Vec::new());
    recovery_pass.album = album_with_session("PrimarySync", "Photos", Box::new(session));
    assert!(!h.config.directory.exists());
    let result = cycle_passes(&h.config, &[recovery_pass]).await;
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "{result:?}"
    );
    assert_transfer_stats(&result.stats, 3, expected_bytes, 2, 1);
    assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
    assert_eq!(result.stats.same_cycle_recovery_successes, 1);
    assert_eq!(result.sync_token.as_deref(), Some("fixture-token"));
    assert_eq!(calls.load(Ordering::SeqCst), 12);
    assert_eq!(
        result.stats.assets_seen, 1,
        "replay inventory stays out of original pass totals"
    );
    assert_eq!(
        result.stats.skipped.total(),
        0,
        "replay skips must not count the original transfer again"
    );
    let metrics = MetricsHandle::new(None);
    assert_reporting(&h, &result.stats, &metrics, expected_bytes).await;
    h.reopen().await;
    let expected = &[
        EXPECTED_FILES[0],
        EXPECTED_FILES[1],
        ("Photos/Movie.MOV", "media/pattern.mov"),
    ];
    h.assert_files(expected).await;
    let unchanged = cycle_passes(&h.config, &[album_pass("Photos", all)]).await;
    assert!(
        matches!(unchanged.outcome, DownloadOutcome::Success),
        "{unchanged:?}"
    );
    assert_transfer_stats(&unchanged.stats, 0, 0, 0, 0);
    assert_eq!(unchanged.stats.same_cycle_recovery_attempts, 0);
    assert_eq!(unchanged.stats.assets_seen, 3);
    assert_eq!(unchanged.stats.skipped.on_disk, 3);
    assert_reporting(&h, &unchanged.stats, &metrics, expected_bytes).await;
    h.reopen().await;
    h.assert_files(expected).await;
    assert!(h.db().get_pending().await.unwrap().is_empty());
    assert!(h.db().get_failed().await.unwrap().is_empty());
    h.server.verify().await;
}
