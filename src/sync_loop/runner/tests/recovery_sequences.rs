#![cfg(test)]

//! Bounded, replayable current identity and metadata recovery histories.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::sync_cycle::run_cycle;
use crate::sync_loop::test_support::{
    MutableCaptureSession, RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions,
    full_album_page_with_download, make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle, media_without_photo_downloads,
};
use crate::{download, state};

// A stale historical receipt alone does not authorize a metadata write or
// manufacture a completed capture, even if provider lookup could return data.
#[tokio::test]
async fn stale_historical_revision_alone_does_not_start_or_complete_capture() {
    const ZONE: &str = "PrimarySync";
    const MASTER: &str = "stale-history";
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let path = media.join("legacy.jpg");
    std::fs::write(&path, b"unchanged legacy bytes").unwrap();
    let sidecar = media.join("unrelated.xmp");
    std::fs::write(&sidecar, b"untouched sidecar").unwrap();
    let original_date = chrono::DateTime::from_timestamp(1_600_000_000, 0).unwrap();
    {
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new(MASTER)
            .library(ZONE)
            .filename("legacy.jpg")
            .created_at(original_date)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(ZONE, MASTER, "original", &path, "local-checksum", None)
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test(ZONE, MASTER, 0);
        db.set_metadata("sync_token:PrimarySync", "old-cursor")
            .await
            .unwrap();
        db.set_metadata(
            crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&make_run_cycle_config()),
        )
        .await
        .unwrap();
        db.acquire_lock("reject unauthorized historical writes").unwrap().execute_batch(
            "CREATE TRIGGER reject_stale_asset_write BEFORE UPDATE ON assets BEGIN SELECT RAISE(ABORT,'stale revision cannot authorize asset writes'); END;
             CREATE TRIGGER reject_stale_receipt_write BEFORE UPDATE ON asset_metadata_capture_revisions BEGIN SELECT RAISE(ABORT,'stale revision cannot authorize receipt writes'); END;"
        ).unwrap();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let page = full_album_page_with_download(
        ZONE,
        MASTER,
        "next-cursor",
        "https://example.invalid/photo.jpg",
        1024,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    );
    let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
    let db = inner.clone() as Arc<dyn download::DownloadStore>;
    let library = make_run_cycle_library_state_with_album(
        ZONE,
        "sync_token:PrimarySync",
        make_full_album_with_boxed_session(
            ZONE,
            Box::new(MutableCaptureSession {
                zone: ZONE,
                records: Arc::new(Mutex::new(page["records"].as_array().unwrap().clone())),
                repair_requests: calls.clone(),
                cancel_after_lookup: None,
            }),
        ),
    );
    let config = make_run_cycle_config();
    let build = make_run_cycle_download_config_builder_with_options(
        &media,
        db.clone(),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &session,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.stats.metadata_capture_refreshed, 0);
    assert_eq!(result.stats.metadata_capture_failures, 0);
    assert_eq!(result.stats.state_write_failures, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(build);
    drop(db);
    drop(inner);
    let db = state::SqliteStateDb::open(&database).await.unwrap();
    let rows = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].created_at, original_date);
    assert_eq!(rows[0].local_path.as_deref(), Some(path.as_path()));
    assert_eq!(std::fs::read(&path).unwrap(), b"unchanged legacy bytes");
    assert_eq!(std::fs::read(&sidecar).unwrap(), b"untouched sidecar");
    assert!(
        db.has_metadata_capture_work(&[ZONE], state::METADATA_CAPTURE_REVISION)
            .await
            .unwrap()
    );
    let conn = db.acquire_lock("inspect stale capture evidence").unwrap();
    let revision: i64 = conn.query_row("SELECT revision FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2", [ZONE, MASTER], |row| row.get(0)).unwrap();
    let retries: i64 = conn
        .query_row(
            "SELECT count(*) FROM metadata_capture_retries WHERE library=?1 AND asset_id=?2",
            [ZONE, MASTER],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revision, 0);
    assert_eq!(retries, 0);
}

#[derive(Clone)]
struct CurrentHistorySession {
    records: Vec<serde_json::Value>,
    lookup: Vec<serde_json::Value>,
    lookups: Arc<AtomicUsize>,
    cancel_after_lookup: Option<CancellationToken>,
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for CurrentHistorySession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.contains("/records/lookup?") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            if let Some(cancel) = &self.cancel_after_lookup {
                cancel.cancel();
            }
            let names = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .lookup
                .iter()
                .filter(|record| {
                    names
                        .iter()
                        .any(|name| name["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":self.records,"syncToken":"after","moreComing":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(crate::sync_loop::test_support::album_count_response(0));
        }
        if url.contains("/records/query?") {
            return Ok(json!({"records":[],"syncToken":"rank-token"}));
        }
        anyhow::bail!("unexpected synthetic current-history request")
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

fn current_history_owner() -> crate::state::db::account::AccountOwner {
    crate::state::db::account::AccountOwner::authenticated(
        "history@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"current-history"}})).unwrap(),
    )
    .unwrap()
}

fn current_history_pass(
    db: &Arc<state::SqliteStateDb>,
    session: CurrentHistorySession,
) -> crate::commands::AlbumPass {
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.invalid"),
            name: Arc::from(""),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            current_history_owner(),
            "com",
        ),
        Arc::from("private"),
    );
    crate::commands::AlbumPass {
        kind: crate::commands::PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    }
}

#[tokio::test]
async fn current_history_observation_admits_scoped_retry_before_reopen() {
    use crate::state::SparseIdentityStore;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&path, &current_history_owner())
            .await
            .unwrap(),
    );
    let record = crate::test_helpers::sparse_shared_asset_record();
    let calls = Arc::new(AtomicUsize::new(0));
    let pass = current_history_pass(
        &db,
        CurrentHistorySession {
            records: vec![record.clone()],
            lookup: vec![record],
            lookups: calls.clone(),
            cancel_after_lookup: None,
        },
    );
    let mut config = download::DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.folder_structure = String::new();
    config.state_db = Some(db.clone());
    config.sync_mode = download::SyncMode::Incremental {
        zone_sync_token: "before".into(),
    };
    let result = download::download_photos_with_sync(
        &reqwest::Client::new(),
        &[pass],
        Arc::new(config),
        download::DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(result.checkpoint.identity_incomplete, "{result:?}");
    assert!(result.sync_token.is_none());
    let rows = db.sparse_identities("PrimarySync").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0]
            .origin
            .as_deref()
            .is_some_and(|origin| origin.starts_with("[\"current-v1\","))
    );
    assert!(rows[0].next_retry.is_some());
    let deadline = rows[0].next_retry;
    drop(rows);
    drop(db);
    let db = state::SqliteStateDb::open_owned(&path, &current_history_owner())
        .await
        .unwrap();
    let rows = db.sparse_identities("PrimarySync").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].next_retry, deadline);
    assert!(calls.load(Ordering::SeqCst) > 0);
}

#[derive(Clone)]
struct CurrentCycleSession {
    inner: CurrentHistorySession,
    cycle: usize,
}
#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for CurrentCycleSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        let request: serde_json::Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"baseline-boundary","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        let mut response = self.inner.post(url, body, headers).await?;
        if url.contains("/changes/zone?") {
            response["zones"][0]["syncToken"] = json!(match self.cycle {
                0 => "baseline-after",
                1 => "unresolved-after",
                2 => "omitted-after",
                3 => "recovered-after",
                _ => "quiet-after",
            });
        }
        Ok(response)
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn current_scoped_retry_run_cycle_holds_then_recovers_and_reopens_quietly() {
    use crate::state::SparseIdentityStore;
    use crate::sync_loop::test_support::{
        make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    };
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    let server = crate::start_wiremock_or_skip!();
    let bytes = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
    Mock::given(method("GET"))
        .and(path("/target.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .expect(1)
        .mount(&server)
        .await;
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let pair = full_album_page_with_download(
        "PrimarySync",
        "target",
        "unused",
        &format!("{}/target.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    let mut source = crate::test_helpers::sparse_shared_asset_record();
    source["recordName"] = json!("asset-target");
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let sidecar = media.join("unrelated.xmp");
    std::fs::write(&sidecar, b"untouched").unwrap();
    let key = crate::sync_cycle::sync_token_key("PrimarySync");
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let lookups = Arc::new(AtomicUsize::new(0));
    let mut recovered_lookups = None;
    for cycle in 0..5 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&database, &current_history_owner())
                .await
                .unwrap(),
        );
        if cycle == 3 {
            let changed = db.acquire_lock("make cycle retry due").unwrap().execute("UPDATE unresolved_sparse_identities SET next_retry_at=0 WHERE epoch=1 AND library='PrimarySync' AND source_record_name='asset-target'", []).unwrap();
            assert_eq!(
                changed, 1,
                "current sparse work must survive the omitted-source reopen"
            );
        }
        let session = CurrentCycleSession {
            cycle,
            inner: CurrentHistorySession {
                records: if cycle == 1 {
                    vec![source.clone()]
                } else {
                    Vec::new()
                },
                lookup: if cycle >= 3 {
                    pair.clone()
                } else {
                    vec![source.clone()]
                },
                lookups: lookups.clone(),
                cancel_after_lookup: None,
            },
        };
        let mut library = crate::icloud::photos::PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session),
            Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            current_history_owner(),
            "com",
        ));
        let pass = crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        };
        let mut selected =
            make_run_cycle_library_state_with_passes("PrimarySync", &key, vec![pass]);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&media, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = String::new();
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let result = run_cycle(
            &[&selected],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let rows = db.sparse_identities("PrimarySync").await.unwrap();
        let cursor = db.get_metadata(&key).await.unwrap();
        match cycle {
            0 => {
                assert!(rows.is_empty());
                assert_eq!(cursor.as_deref(), Some("baseline-after"));
            }
            1 | 2 => {
                assert!(
                    result.stats.identity_incomplete,
                    "unresolved cycle must block identity completion"
                );
                assert_eq!(rows.len(), 1, "source retry survives reopen");
                assert_eq!(
                    cursor.as_deref(),
                    Some("baseline-after"),
                    "source checkpoint remains held"
                );
            }
            _ => {
                assert!(
                    rows.is_empty(),
                    "source retirement requires the cycle completion proof"
                );
                assert_eq!(
                    cursor.as_deref(),
                    Some(if cycle == 3 {
                        "recovered-after"
                    } else {
                        "quiet-after"
                    })
                );
                let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(downloaded.len(), 1);
                assert_eq!(
                    std::fs::read(downloaded[0].local_path.as_ref().unwrap()).unwrap(),
                    bytes
                );
                assert_eq!(result.stats.downloaded, usize::from(cycle == 3));
                if let Some(previous) = recovered_lookups {
                    assert_eq!(
                        lookups.load(Ordering::SeqCst),
                        previous,
                        "quiet cycle must not retry identity"
                    );
                }
                recovered_lookups = Some(lookups.load(Ordering::SeqCst));
            }
        }
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"untouched");
    }
    server.verify().await;
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Event {
    #[serde(alias = "Ambiguous")]
    Unresolved,
    Deferred,
    MissingDate,
    NullDate,
    OutOfRangeDate,
    MissingChild,
    OmittedSource,
    VisibleInvalid,
    #[serde(alias = "FilteredInvalid")]
    RecentInvalid,
    InterruptLookup,
    FailRefresh,
    Recover,
    Steady,
}

// A source location is a stable discriminator when reducing generated faults;
// step indices and changing result details must not become part of the signature.
macro_rules! history_assert {
    ($condition:expr $(,)?) => { assert!($condition, concat!("history assertion at ", file!(), ":", line!())) };
    ($condition:expr, $($message:tt)+) => { assert!($condition, $($message)+) };
}
macro_rules! history_assert_eq {
    ($left:expr, $right:expr $(,)?) => { assert_eq!($left, $right, concat!("history equality at ", file!(), ":", line!())) };
    ($left:expr, $right:expr, $($message:tt)+) => { assert_eq!($left, $right, $($message)+) };
}

async fn run_current_sparse_history(trace: &[Event]) {
    use crate::state::SparseIdentityStore;
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    let server = crate::start_wiremock_or_skip!();
    let bytes = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9";
    Mock::given(method("GET"))
        .and(path("/target.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .expect(1)
        .mount(&server)
        .await;
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let pair = full_album_page_with_download(
        "PrimarySync",
        "target",
        "after",
        &format!("{}/target.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    let mut source = crate::test_helpers::sparse_shared_asset_record();
    source["recordName"] = json!("asset-target");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let old = media.join("old.bin");
    let sidecar = media.join("unrelated.xmp");
    std::fs::write(&old, b"existing bytes").unwrap();
    std::fs::write(&sidecar, b"unrelated sidecar").unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&path, &current_history_owner())
            .await
            .unwrap();
        db.set_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"), "before")
            .await
            .unwrap();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let mut after_recovery_calls = None;
    for (step, event) in trace.iter().copied().enumerate() {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&path, &current_history_owner())
                .await
                .unwrap(),
        );
        let completed = matches!(event, Event::Recover | Event::Steady);
        if step >= 2 && !matches!(event, Event::Steady) {
            let changed = db.acquire_lock("make the current sparse retry due").unwrap().execute(
                "UPDATE unresolved_sparse_identities SET next_retry_at=0 WHERE epoch=1 AND library='PrimarySync' AND source_record_name='asset-target'", []).unwrap();
            history_assert_eq!(
                changed,
                1,
                "exact current sparse retry must exist before advancing its deadline"
            );
        }
        let before_calls = calls.load(Ordering::SeqCst);
        let before_rows = db.sparse_identities("PrimarySync").await.unwrap();
        let deadline = before_rows.first().and_then(|row| row.next_retry);
        let cancel = CancellationToken::new();
        let session = CurrentHistorySession {
            records: if matches!(event, Event::Steady | Event::OmittedSource) {
                vec![]
            } else {
                vec![source.clone()]
            },
            lookup: if completed {
                pair.clone()
            } else if matches!(event, Event::MissingChild) {
                vec![]
            } else {
                vec![source.clone()]
            },
            lookups: calls.clone(),
            cancel_after_lookup: matches!(event, Event::InterruptLookup).then(|| cancel.clone()),
        };
        let pass = current_history_pass(&db, session);
        let cursor = db
            .get_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"))
            .await
            .unwrap()
            .unwrap();
        let mut config = download::DownloadConfig::test_default();
        config.directory = Arc::from(media.as_path());
        config.folder_structure = String::new();
        config.state_db = Some(db.clone());
        config.sync_mode = download::SyncMode::Incremental {
            zone_sync_token: cursor,
        };
        let result = download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[pass],
            Arc::new(config),
            download::DownloadControls::download_hidden(),
            cancel.clone(),
        )
        .await
        .unwrap();
        if completed {
            history_assert!(
                !result.checkpoint.identity_incomplete,
                "resolved identity must have completion evidence"
            );
            let successor = result
                .sync_token
                .clone()
                .expect("resolved current source has a successor");
            db.commit_checkpoint_transition(state::CheckpointTransition {
                expected_metadata: Vec::new(),
                legacy_preservation_proofs: Vec::new(),
                legacy_config_hash: None,
                sparse_identity_proofs: result.checkpoint.sparse_identity_proofs.clone(),
                metadata_updates: vec![(
                    crate::sync_cycle::sync_token_key("PrimarySync"),
                    successor,
                )],
                metadata_deletes: Vec::new(),
            })
            .await
            .unwrap();
            history_assert!(
                db.sparse_identities("PrimarySync")
                    .await
                    .unwrap()
                    .is_empty()
            );
            let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
            history_assert_eq!(downloaded.len(), 1, "one target must be durably published");
            history_assert_eq!(
                std::fs::read(downloaded[0].local_path.as_ref().unwrap()).unwrap(),
                bytes,
                "published target bytes must survive reopen"
            );
            history_assert_eq!(
                result.stats.downloaded,
                usize::from(matches!(event, Event::Recover)),
                "target must be downloaded exactly once"
            );
            if let Some(previous) = after_recovery_calls {
                history_assert_eq!(
                    calls.load(Ordering::SeqCst),
                    previous,
                    "quiet tail must not retry identity"
                );
            }
            after_recovery_calls = Some(calls.load(Ordering::SeqCst));
        } else {
            history_assert!(
                result.checkpoint.identity_incomplete || cancel.is_cancelled(),
                "unresolved source must hold identity completion"
            );
            history_assert!(result.sync_token.is_none());
            history_assert_eq!(
                db.get_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("before")
            );
            let rows = db.sparse_identities("PrimarySync").await.unwrap();
            history_assert_eq!(
                rows.len(),
                1,
                "exactly one scoped source must remain unresolved"
            );
            history_assert!(
                rows[0]
                    .origin
                    .as_deref()
                    .is_some_and(|origin| origin.starts_with("[\"current-v1\","))
            );
            if matches!(event, Event::Deferred) {
                history_assert!(deadline.is_some());
                history_assert_eq!(rows[0].next_retry, deadline);
                history_assert_eq!(calls.load(Ordering::SeqCst), before_calls);
            }
            if matches!(
                event,
                Event::MissingChild | Event::OmittedSource | Event::InterruptLookup
            ) {
                history_assert!(
                    calls.load(Ordering::SeqCst) > before_calls,
                    "fault must reach source lookup"
                );
            }
            if matches!(event, Event::MissingChild) {
                history_assert!(
                    rows[0].next_retry.is_none(),
                    "inconclusive lookup has no stable-negative deadline"
                );
            }
            if matches!(event, Event::InterruptLookup) {
                history_assert!(cancel.is_cancelled());
            }
        }
        history_assert_eq!(std::fs::read(&old).unwrap(), b"existing bytes");
        history_assert_eq!(std::fs::read(&sidecar).unwrap(), b"unrelated sidecar");
    }
    server.verify().await;
}

async fn run_current_metadata_history(trace: &[Event]) {
    use crate::state::DownloadStateStore as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let sidecar = media.join("unrelated.xmp");
    std::fs::write(&sidecar, b"unrelated sidecar").unwrap();
    let mut original = full_album_page_with_download(
        "PrimarySync",
        "metadata-target",
        "after",
        &format!("{}/media.jpg", server.uri()),
        32,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    original[1]["fields"]["assetDate"]["value"] = json!(1_600_000_000_000i64);
    original[1]["fields"]["isFavorite"] = json!({"value":0,"type":"INT64"});
    original[1]["fields"]["isHidden"] = json!({"value":0,"type":"INT64"});
    let asset = crate::icloud::photos::PhotoAsset::new(original[0].clone(), original[1].clone());
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&database, &current_history_owner())
            .await
            .unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let pass = current_history_pass(
        &db,
        CurrentHistorySession {
            records: Vec::new(),
            lookup: Vec::new(),
            lookups: calls.clone(),
            cancel_after_lookup: None,
        },
    );
    let mut config = download::DownloadConfig::test_default();
    config.directory = Arc::from(media.as_path());
    config.folder_structure = String::new();
    let pass_config = config.with_pass(&pass);
    let expected = crate::download::filter::expected_paths_for(&asset, &pass_config)
        .into_iter()
        .next()
        .unwrap();
    let source = vec![0u8; expected.size as usize];
    std::fs::create_dir_all(expected.path.parent().unwrap()).unwrap();
    std::fs::write(&expected.path, &source).unwrap();
    let record = crate::test_helpers::TestAssetRecord::new(asset.state_id())
        .library("PrimarySync")
        .checksum(&expected.checksum)
        .filename(expected.path.file_name().unwrap().to_str().unwrap())
        .created_at(asset.created())
        .added_at(asset.added_date())
        .size(expected.size)
        .version_size(expected.version_size)
        .metadata(
            (*crate::download::filter::metadata_for_selected_version(
                &asset,
                &pass_config,
                expected.version_size,
            ))
            .clone(),
        )
        .build();
    db.upsert_seen(&record).await.unwrap();
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(&source));
    db.mark_verified_download(
        "PrimarySync",
        asset.state_id(),
        expected.version_size.as_str(),
        &expected.path,
        &digest,
        Some(&digest),
        false,
    )
    .await
    .unwrap();
    let key = crate::sync_cycle::sync_token_key("PrimarySync");
    db.set_metadata(&key, "before").await.unwrap();
    drop(pass);
    drop(db);
    let original_date = asset.created();
    let mut recovered = false;
    for event in trace.iter().copied() {
        if !matches!(
            event,
            Event::MissingDate
                | Event::NullDate
                | Event::OutOfRangeDate
                | Event::VisibleInvalid
                | Event::RecentInvalid
                | Event::FailRefresh
                | Event::Recover
                | Event::Steady
        ) {
            continue;
        }
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&database, &current_history_owner())
                .await
                .unwrap(),
        );
        db.acquire_lock("configure current metadata fault")
            .unwrap()
            .execute_batch("DROP TRIGGER IF EXISTS fail_current_metadata_refresh")
            .unwrap();
        let mut changed = original.clone();
        changed[1]["fields"]["assetDate"]["value"] = json!(RUN_CYCLE_ASSET_DATE_MS);
        changed[1]["fields"]["isFavorite"]["value"] = json!(1);
        changed[1]["fields"]["isHidden"]["value"] = json!(0);
        match event {
            Event::MissingDate | Event::RecentInvalid => {
                changed[1]["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("assetDate");
            }
            Event::VisibleInvalid => {
                changed[1]["fields"]["assetDate"]["value"] = json!("invalid-current-date")
            }
            Event::NullDate => changed[1]["fields"]["assetDate"]["value"] = json!(null),
            Event::OutOfRangeDate => changed[1]["fields"]["assetDate"]["value"] = json!(1e100),
            _ => {}
        }
        if matches!(event, Event::VisibleInvalid) {
            changed[1]["fields"]["isHidden"]["value"] = json!(0);
        }
        if matches!(event, Event::FailRefresh) {
            db.acquire_lock("fail current metadata update").unwrap().execute_batch("CREATE TRIGGER fail_current_metadata_refresh BEFORE UPDATE OF created_at ON assets BEGIN SELECT RAISE(ABORT,'synthetic current metadata failure'); END;").unwrap();
        }
        let records = if matches!(event, Event::Steady) {
            Vec::new()
        } else {
            changed
        };
        let pass = current_history_pass(
            &db,
            CurrentHistorySession {
                records,
                lookup: Vec::new(),
                lookups: calls.clone(),
                cancel_after_lookup: None,
            },
        );
        let cursor = db.get_metadata(&key).await.unwrap().unwrap();
        config.state_db = Some(db.clone());
        config.recent = matches!(event, Event::RecentInvalid).then_some(1);
        config.sync_mode = download::SyncMode::Incremental {
            zone_sync_token: cursor,
        };
        let result = download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[pass],
            Arc::new(config.clone()),
            download::DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        if matches!(event, Event::Recover | Event::Steady) {
            let token = result
                .sync_token
                .clone()
                .expect("valid current metadata has a source successor");
            history_assert!(
                !result.checkpoint.sync_token_blocked,
                "current metadata checkpoint must respect the recorded outcome"
            );
            db.commit_checkpoint_transition(state::CheckpointTransition {
                expected_metadata: Vec::new(),
                legacy_preservation_proofs: Vec::new(),
                legacy_config_hash: None,
                sparse_identity_proofs: result.checkpoint.sparse_identity_proofs.clone(),
                metadata_updates: vec![(key.clone(), token)],
                metadata_deletes: Vec::new(),
            })
            .await
            .unwrap();
            recovered = true;
        } else {
            history_assert!(
                result.sync_token.is_none(),
                "current metadata checkpoint must respect the recorded outcome"
            );
            history_assert!(
                result.checkpoint.sync_token_blocked,
                "current metadata checkpoint must respect the recorded outcome"
            );
            history_assert_eq!(
                db.get_metadata(&key).await.unwrap().as_deref(),
                Some("before")
            );
            if matches!(event, Event::FailRefresh) {
                history_assert!(
                    result.stats.state_write_failures > 0,
                    "refresh failure must reach state write"
                );
            } else {
                history_assert_eq!(
                    result.stats.sync_token_blocked_reason,
                    Some(crate::icloud::photos::asset::MALFORMED_REQUIRED_ASSET_FIELDS_REASON),
                    "malformed current date must be the checkpoint veto"
                );
                history_assert_eq!(
                    result.stats.state_write_failures,
                    0,
                    "invalid date must not reach the metadata writer"
                );
            }
        }
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        history_assert_eq!(rows.len(), 1);
        history_assert_eq!(
            rows[0].created_at,
            if recovered {
                chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap()
            } else {
                original_date
            }
        );
        history_assert_eq!(rows[0].metadata.is_favorite, recovered);
        history_assert_eq!(rows[0].local_path.as_deref(), Some(expected.path.as_path()));
        history_assert_eq!(std::fs::read(&expected.path).unwrap(), source);
        history_assert_eq!(std::fs::read(&sidecar).unwrap(), b"unrelated sidecar");
        history_assert_eq!(result.stats.downloaded, 0);
        config.state_db = None;
    }
    server.verify().await;
}

// A hidden delta is an identity-only state transition, not a capture refresh.
// Its own write must still be durable before its successor is committed.
#[tokio::test]
async fn hidden_identity_only_date_preserves_metadata_and_retries_failed_write() {
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/hidden.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let key = crate::sync_cycle::sync_token_key("PrimarySync");
    let original_date = chrono::DateTime::from_timestamp(1_600_000_000, 0).unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&database, &current_history_owner())
            .await
            .unwrap();
        for name in ["hidden-target", "unrelated"] {
            let path = media.join(format!("{name}.jpg"));
            std::fs::write(&path, name.as_bytes()).unwrap();
            let record = crate::test_helpers::TestAssetRecord::new(name)
                .library("PrimarySync")
                .filename(&format!("{name}.jpg"))
                .created_at(original_date)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                name,
                "original",
                &path,
                "local-checksum",
                None,
            )
            .await
            .unwrap();
        }
        db.set_metadata(&key, "before").await.unwrap();
        db.acquire_lock("reject hidden transition").unwrap().execute_batch("CREATE TRIGGER reject_hidden_identity BEFORE UPDATE OF is_hidden ON assets BEGIN SELECT RAISE(ABORT,'synthetic hidden state failure'); END;").unwrap();
    }
    let mut pair = full_album_page_with_download(
        "PrimarySync",
        "hidden-target",
        "after",
        &format!("{}/hidden.jpg", server.uri()),
        32,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    pair[1]["fields"]
        .as_object_mut()
        .unwrap()
        .remove("assetDate");
    pair[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
    pair[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
    let calls = Arc::new(AtomicUsize::new(0));
    for cycle in 0..3 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&database, &current_history_owner())
                .await
                .unwrap(),
        );
        if cycle == 1 {
            db.acquire_lock("allow hidden transition")
                .unwrap()
                .execute_batch("DROP TRIGGER reject_hidden_identity")
                .unwrap();
        }
        let pass = current_history_pass(
            &db,
            CurrentHistorySession {
                records: if cycle < 2 { pair.clone() } else { Vec::new() },
                lookup: Vec::new(),
                lookups: calls.clone(),
                cancel_after_lookup: None,
            },
        );
        let mut config = download::DownloadConfig::test_default();
        config.directory = Arc::from(media.as_path());
        config.folder_structure = String::new();
        config.state_db = Some(db.clone());
        config.sync_mode = download::SyncMode::Incremental {
            zone_sync_token: db.get_metadata(&key).await.unwrap().unwrap(),
        };
        let result = download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[pass],
            Arc::new(config),
            download::DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        if cycle == 0 {
            assert!(result.checkpoint.sync_token_blocked, "{result:?}");
            assert!(result.stats.state_write_failures > 0);
            assert!(result.sync_token.is_none());
            assert_eq!(
                db.get_metadata(&key).await.unwrap().as_deref(),
                Some("before")
            );
        } else {
            let token = result
                .sync_token
                .expect("completed hidden transition has source successor");
            db.commit_checkpoint_transition(state::CheckpointTransition {
                expected_metadata: Vec::new(),
                legacy_preservation_proofs: Vec::new(),
                legacy_config_hash: None,
                sparse_identity_proofs: result.checkpoint.sparse_identity_proofs.clone(),
                metadata_updates: vec![(key.clone(), token)],
                metadata_deletes: Vec::new(),
            })
            .await
            .unwrap();
            assert_eq!(
                db.get_metadata(&key).await.unwrap().as_deref(),
                Some("after")
            );
        }
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.created_at, original_date);
            assert!(
                !row.metadata.is_favorite,
                "hidden identity-only data cannot fabricate a metadata refresh"
            );
            assert_eq!(
                row.metadata.is_hidden,
                row.id.as_ref() == "hidden-target" && cycle > 0
            );
            assert_eq!(
                std::fs::read(row.local_path.as_ref().unwrap()).unwrap(),
                row.id.as_bytes()
            );
        }
        assert_eq!(result.stats.downloaded, 0);
    }
    server.verify().await;
}

#[tokio::test]
async fn current_metadata_history_probe() {
    run_current_metadata_history(&[
        Event::MissingDate,
        Event::NullDate,
        Event::OutOfRangeDate,
        Event::VisibleInvalid,
        Event::RecentInvalid,
        Event::FailRefresh,
        Event::Recover,
        Event::Steady,
        Event::Steady,
    ])
    .await;
}

#[tokio::test]
async fn current_sparse_history_probe() {
    run_current_sparse_history(&[
        Event::Unresolved,
        Event::Deferred,
        Event::MissingChild,
        Event::InterruptLookup,
        Event::Recover,
        Event::Steady,
        Event::Steady,
    ])
    .await;
}

// Independent current operations share the fault schedule but never share proof:
// sparse identity uses its own scoped retry, while metadata uses the source cursor.
async fn run_current_recovery_history(seed: u64, trace: &[Event]) {
    eprintln!("current recovery seed={seed} trace={trace:?}");
    run_current_sparse_history(trace).await;
    run_current_metadata_history(trace).await;
}

async fn run_current_recovery_sequence(seed: u64) {
    // The prefix records a scoped unresolved source and exercises deferral and
    // a separate current-metadata write failure. Every shuffle adds faults.
    // Recovery is deliberately separate from fault selection, so liveness has
    // a fixed bound and cannot be excused by an unlucky generated schedule.
    let mut faults = [
        Event::MissingDate,
        Event::NullDate,
        Event::OutOfRangeDate,
        Event::InterruptLookup,
        Event::FailRefresh,
    ];
    faults.shuffle(&mut rand::rngs::StdRng::seed_from_u64(seed));
    let mut trace = vec![Event::Unresolved, Event::Deferred, Event::FailRefresh];
    trace.extend(faults);
    trace.extend([Event::Recover, Event::Steady, Event::Steady]);

    Box::pin(run_current_recovery_history(seed, &trace)).await;
}

#[tokio::test]
async fn current_recovery_sequence_seed_862() {
    Box::pin(run_current_recovery_sequence(862)).await;
}
#[tokio::test]
async fn current_recovery_sequence_seed_873() {
    Box::pin(run_current_recovery_sequence(873)).await;
}
#[tokio::test]
async fn current_recovery_sequence_seed_869() {
    Box::pin(run_current_recovery_sequence(869)).await;
}
#[tokio::test]
async fn current_recovery_sequence_seed_870() {
    Box::pin(run_current_recovery_sequence(870)).await;
}

// Synthetic current-work scenarios are inspired by public issue themes, not
// claimed reproductions. No captured provider traffic, account DBs or photos.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconstructedHistory {
    name: String,
    provenance: String,
    public_issues: Vec<u64>,
    relationships: String,
    faults: Vec<Event>,
}

fn with_recovery_tail(faults: &[Event]) -> Vec<Event> {
    let mut trace = vec![Event::Unresolved, Event::Deferred, Event::FailRefresh];
    trace.extend_from_slice(faults);
    trace.extend([Event::Recover, Event::Steady, Event::Steady]);
    trace
}

fn generated_faults(seed: u64) -> Vec<Event> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let vocabulary = [
        Event::MissingDate,
        Event::NullDate,
        Event::OutOfRangeDate,
        Event::InterruptLookup,
        Event::FailRefresh,
        Event::MissingChild,
        Event::OmittedSource,
        Event::VisibleInvalid,
        Event::RecentInvalid,
        Event::Unresolved,
    ];
    // Draw with replacement and vary length, rather than shuffling a fixed bag.
    // Repeated errors and oscillating evidence survive every SQLite reopen.
    (0..rng.random_range(1..=18))
        .map(|_| vocabulary[rng.random_range(0..vocabulary.len())])
        .collect()
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else {
        "non-string panic".to_string()
    }
}

async fn check_current_history(seed: u64, faults: &[Event]) {
    assert!(
        faults.len() <= 18,
        "replay retains the generated length bound"
    );
    assert!(
        faults
            .iter()
            .all(|event| !matches!(event, Event::Deferred | Event::Recover | Event::Steady)),
        "replay faults cannot replace prerequisite or liveness events"
    );
    let trace = with_recovery_tail(faults);
    let result = std::panic::AssertUnwindSafe(run_current_recovery_history(seed, &trace))
        .catch_unwind()
        .await;
    let Err(original) = result else { return };
    let signature = panic_message(original.as_ref())
        .lines()
        .next()
        .unwrap_or("non-string panic")
        .to_owned();
    let mut reduced = faults.to_vec();
    let mut attempts = 0;
    let mut index = 0;
    // Bounded deletion reduction preserves the prerequisite state and mandatory
    // valid/quiet tail. Stable oracle labels distinguish assertion failures.
    while index < reduced.len() && attempts < 24 {
        let mut candidate = reduced.clone();
        candidate.remove(index);
        let candidate_trace = with_recovery_tail(&candidate);
        attempts += 1;
        let result =
            std::panic::AssertUnwindSafe(run_current_recovery_history(seed, &candidate_trace))
                .catch_unwind()
                .await;
        if result.as_ref().is_err_and(|payload| {
            panic_message(payload.as_ref()).lines().next() == Some(signature.as_str())
        }) {
            reduced = candidate;
            index = 0;
        } else {
            index += 1;
        }
    }
    eprintln!(
        "Replay reduced faults with KEI_RECOVERY_FAULTS='{}' cargo test --lib current_generated_histories -- --nocapture",
        serde_json::to_string(&reduced).unwrap()
    );
    eprintln!(
        "RECOVERY FAILURE seed={seed} original={trace:?} reduced={:?} deletion_attempts={attempts} assertion={signature}",
        with_recovery_tail(&reduced)
    );
    std::panic::resume_unwind(original);
}

#[tokio::test]
async fn current_reconstructed_histories() {
    let cases: Vec<ReconstructedHistory> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/recovery-histories.json"
    )))
    .unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        assert_eq!(case.provenance, "synthetic-current-work");
        assert!(!case.public_issues.is_empty());
        assert!(!case.relationships.is_empty());
        eprintln!(
            "reconstructed case={} issues={:?} relationships={}",
            case.name, case.public_issues, case.relationships
        );
        Box::pin(check_current_history(862, &case.faults)).await;
    }
}

#[tokio::test]
async fn current_generated_histories() {
    // Keep the default gate bounded. The full printed trace is the replay
    // contract, since RNG algorithms can change across dependency upgrades.
    if let Ok(faults) = std::env::var("KEI_RECOVERY_FAULTS") {
        let faults: Vec<Event> = serde_json::from_str(&faults).expect("JSON event array");
        Box::pin(check_current_history(0, &faults)).await;
    } else {
        let seeds = std::env::var("KEI_RECOVERY_SEED").map_or_else(
            |_| vec![0, 1, 42, 765, 853, 861, 862, 20261001],
            |seed| vec![seed.parse::<u64>().expect("unsigned recovery seed")],
        );
        for seed in seeds {
            Box::pin(check_current_history(seed, &generated_faults(seed))).await;
        }
    }
}
