//! A failing current transfer must not starve an independent historical retry.
use super::{BaselineSession, MEDIA, ZONE, baseline_owner, generation, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone)]
struct CurrentView(BaselineSession, String);

#[async_trait::async_trait]
impl PhotosSession for CurrentView {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if !url.contains("/records/lookup?") {
            return self.0.post(url, body, headers).await;
        }
        let request: Value = serde_json::from_str(&body)?;
        self.0
            .requests
            .lock()
            .unwrap()
            .push((url.into(), request.clone()));
        // Changes describe observations; lookup returns the authoritative
        // current view once per identity, never both revisions of an asset.
        let mut current = std::collections::BTreeMap::new();
        for record in self.0.inventory.iter().chain(self.0.concurrent.iter()) {
            current.insert(
                record["recordName"].as_str().unwrap().to_owned(),
                record.clone(),
            );
        }
        if self.0.cycle > 0 {
            for record in self.0.incremental_addition.iter() {
                current.insert(
                    record["recordName"].as_str().unwrap().to_owned(),
                    record.clone(),
                );
            }
        }
        // This old pending identity is looked up independently. It is not a
        // newly observed record in the current baseline or change stream.
        let historical = pair("legacy", "legacy.JPG", &self.1, 31);
        for record in historical {
            current.insert(record["recordName"].as_str().unwrap().to_owned(), record);
        }
        let records: Vec<_> = request["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|requested| {
                current
                    .get(requested["recordName"].as_str().unwrap())
                    .cloned()
            })
            .collect();
        Ok(json!({"records": records}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_failed_current_transfer_does_not_starve_legacy_pending_retry() {
    for database in ["private", "shared"] {
        historical_pending(database, false).await;
    }
}

#[tokio::test]
async fn v027_retained_retry_deferral_write_failure_keeps_cursor() {
    historical_pending("private", true).await;
}

async fn historical_pending(database: &'static str, fail_deferral: bool) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let server = crate::start_wiremock_or_skip!();
    for endpoint in ["/second", "/concurrent", "/legacy"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let sentinel = root.join("unrelated-existing.bin");
    std::fs::write(&sentinel, b"existing untouched bytes").unwrap();
    let sentinel_mtime = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    let db_path = dir.path().join("state.db");
    let first = pair("first", "first.JPG", &format!("{}/first", server.uri()), 11);
    let mut inventory = first.clone();
    inventory.extend(pair(
        "second",
        "second.JPG",
        &format!("{}/second", server.uri()),
        12,
    ));
    let concurrent = pair(
        "concurrent",
        "concurrent.JPG",
        &format!("{}/concurrent", server.uri()),
        13,
    );
    let mut changed = first;
    changed[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    let mut legacy_publication = None;
    let mut failed_attempts = Vec::new();
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 3 {
            // Preserve one quiet reopen under backoff, then make the original
            // owner due without changing its admission or destination.
            db.make_selection_retry_due(baseline_owner(), "asset-first".into())
                .await
                .unwrap();
            if fail_deferral {
                db.acquire_lock("refuse retained retry write")
                    .unwrap()
                    .execute_batch(
                        "CREATE TRIGGER refuse_retained_deferral BEFORE UPDATE OF next_retry_at
                     ON provider_active_decisions
                     WHEN NEW.reason='current_confirmation_unavailable' BEGIN
                     SELECT RAISE(ABORT,'synthetic retained retry deferral failure'); END;",
                    )
                    .unwrap();
            }
        }
        if cycle == 1 {
            let record = crate::test_helpers::TestAssetRecord::new("asset-legacy")
                .library(ZONE)
                .filename("legacy.JPG")
                .checksum(&generation(31))
                .size(MEDIA.len() as u64)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.upsert_asset_master_mapping(ZONE, "asset-legacy", "legacy")
                .await
                .unwrap();
        }
        let session = BaselineSession {
            database,
            cycle,
            inventory: Arc::new(inventory.clone()),
            concurrent: Arc::new(concurrent.clone()),
            incremental_addition: Arc::new(changed.clone()),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            format!("https://synthetic.example/database/1/photos/production/{database}"),
            Arc::new(std::collections::HashMap::new()),
            Box::new(CurrentView(
                session.clone(),
                format!("{}/legacy", server.uri()),
            )),
            Arc::new(zone()),
            database.into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await;
        if cycle == 3 && fail_deferral {
            let error = result.expect_err("failed durable deferral must abort progress");
            assert!(format!("{error:#}").contains("synthetic retained retry deferral failure"));
            assert_eq!(
                db.get_metadata(&key).await.unwrap(),
                Some("v27-after-cycle-3".into())
            );
            let pending: i64 = db
                .acquire_lock("failed deferral keeps owner")
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM provider_active_destinations WHERE library=?1
                 AND asset_id='asset-first' AND admitted=1 AND verified_media=0",
                    [ZONE],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(pending > 0);
            assert!(!root.join("first.JPG").exists());
            let (bytes, mtime) = legacy_publication.as_ref().unwrap();
            assert_eq!(&std::fs::read(root.join("legacy.JPG")).unwrap(), bytes);
            assert_eq!(
                &std::fs::metadata(root.join("legacy.JPG"))
                    .unwrap()
                    .modified()
                    .unwrap(),
                mtime
            );
            assert_eq!(
                std::fs::read(&sentinel).unwrap(),
                b"existing untouched bytes"
            );
            assert_eq!(
                std::fs::metadata(&sentinel).unwrap().modified().unwrap(),
                sentinel_mtime
            );
            server.verify().await;
            return;
        }
        let result = result.unwrap();
        if cycle == 2 {
            assert_eq!(result.failed_count, 0, "quiet owner is still under backoff");
        } else {
            assert!(
                result.failed_count > 0,
                "due current transfer must still fail: cycle {cycle}: {result:?}; trace={:?}",
                trace.events()
            );
        }
        assert!(!result.stats.identity_incomplete, "{result:?}");
        if cycle == 3 && !fail_deferral {
            assert!(
                result.stats.skipped.uncertain > 0,
                "changed historical proof stays visible and pending: {result:?}"
            );
        }
        let requests = server.received_requests().await.unwrap();
        failed_attempts.push(requests.iter().filter(|r| r.url.path() == "/first").count());
        assert!(!root.join("first.JPG").exists());
        {
            let conn = db.acquire_lock("independent retained owners").unwrap();
            let current_pending: i64 = conn.query_row(
                "SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND asset_id='asset-first' AND admitted=1 AND verified_media=0",
                [ZONE], |r| r.get(0)).unwrap();
            assert!(
                current_pending > 0,
                "failed current owner must remain durable"
            );
            if cycle > 0 {
                let downloaded: i64 = conn.query_row(
                    "SELECT count(*) FROM assets WHERE library=?1 AND id='asset-legacy' AND status='downloaded'",
                    [ZONE], |r| r.get(0)).unwrap();
                assert_eq!(
                    downloaded,
                    1,
                    "legacy retry must finish despite unrelated failure: cycle {cycle}: {result:?}; trace={:?}",
                    trace.events()
                );
            }
        }
        if cycle > 0 {
            let output = root.join("legacy.JPG");
            let bytes = std::fs::read(&output).unwrap();
            assert_eq!(bytes, MEDIA);
            let mtime = std::fs::metadata(&output).unwrap().modified().unwrap();
            if let Some((prior_bytes, prior_mtime)) = &legacy_publication {
                assert_eq!(&bytes, prior_bytes);
                assert_eq!(&mtime, prior_mtime);
            }
            legacy_publication = Some((bytes, mtime));
            assert_eq!(session.inventory_queries.load(Ordering::SeqCst), 0);
            assert_eq!(session.boundaries.load(Ordering::SeqCst), 0);
        }
        assert_eq!(
            result.stats.downloaded,
            match cycle {
                0 => 2,
                1 => 1,
                _ => 0,
            },
            "{result:?}"
        );
        assert!(
            result.db_sync_token_advance_safe,
            "cycle={cycle}: {result:?}; trace={:?}",
            trace.events()
        );
        assert_eq!(
            db.get_metadata(&key).await.unwrap(),
            Some(format!("v27-after-cycle-{}", cycle + 1))
        );
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"existing untouched bytes"
        );
        assert_eq!(
            std::fs::metadata(&sentinel).unwrap().modified().unwrap(),
            sentinel_mtime
        );
    }
    // Quiet cycles do not bypass the active generation's own attempt policy
    // when unrelated asset-ledger recovery becomes eligible.
    let retry_counts: Vec<_> = failed_attempts.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(
        retry_counts,
        // One initial attempt and the existing bounded cleanup attempt,
        // with no requests on the quiet reopen under backoff.
        [2, 0, 2],
        "independent legacy work cannot bypass current-owner backoff: {failed_attempts:?}"
    );
    server.verify().await;
}
