//! Interrupted current smart-selection work must survive reopening owned state.
use super::super::{MEDIA, ZONE, baseline_owner, pair, zone};
use super::{Selection, Session};
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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Checkpoint,
    MalformedPage,
    EarlyCancel,
    LateCancel,
    DeferredTransfer,
    DeferredSelection,
    StalePlan,
    DownloadedStatus,
}

#[derive(Clone)]
struct InterruptedSession {
    inner: Session,
    fault: Option<Fault>,
    cancel: CancellationToken,
    published: PathBuf,
    hit: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PhotosSession for InterruptedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if self.fault == Some(Fault::DeferredSelection) && url.contains("/records/lookup?") {
            self.hit.store(true, Ordering::SeqCst);
            return Ok(json!({"records":[]}));
        }
        if url.contains("/records/query?") && request["query"]["recordType"] != "CheckIndexingState"
        {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            if self.fault == Some(Fault::EarlyCancel) {
                self.hit.store(true, Ordering::SeqCst);
                self.cancel.cancel();
            }
            if offset > 0 && matches!(self.fault, Some(Fault::MalformedPage | Fault::LateCancel)) {
                // Wait for a real publication, not just a page response, before
                // interrupting the later page. The bounded wait fails if the
                // intended admission boundary was never reached.
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !self.published.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("first page must publish before later-page fault");
                self.hit.store(true, Ordering::SeqCst);
                if self.fault == Some(Fault::MalformedPage) {
                    return Ok(
                        json!({"records":"malformed record list","syncToken":"not-authority"}),
                    );
                }
                self.cancel.cancel();
            }
        }
        self.inner.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_smart_checkpoint_write_failure_reopens_without_duplicate_transfer() {
    interrupted(Fault::Checkpoint).await;
}
#[tokio::test]
async fn v027_smart_malformed_later_page_preserves_admitted_work_on_reopen() {
    interrupted(Fault::MalformedPage).await;
}
#[tokio::test]
async fn v027_smart_early_cancellation_preserves_prior_work_on_reopen() {
    interrupted(Fault::EarlyCancel).await;
}
#[tokio::test]
async fn v027_smart_late_cancellation_preserves_admitted_work_on_reopen() {
    interrupted(Fault::LateCancel).await;
}
#[tokio::test]
async fn v027_changed_smart_selection_deferred_transfer_retries_when_provider_quiet() {
    interrupted(Fault::DeferredTransfer).await;
}

#[tokio::test]
async fn v027_unconfirmed_smart_selection_holds_cursor_then_replays_on_reopen() {
    interrupted(Fault::DeferredSelection).await;
}

#[tokio::test]
async fn v027_stale_smart_plan_preserves_cursor_and_recovers() {
    interrupted(Fault::StalePlan).await;
}
#[tokio::test]
async fn v027_downloaded_status_write_failure_recovers_publication_without_redownload() {
    interrupted(Fault::DownloadedStatus).await;
}

async fn interrupted(fault: Fault) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/original"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let fail_transfer = Arc::new(AtomicBool::new(false));
    let successful_next = Arc::new(AtomicUsize::new(0));
    let failed_next = Arc::new(AtomicUsize::new(0));
    let fail_flag = fail_transfer.clone();
    let success_count = successful_next.clone();
    let failure_count = failed_next.clone();
    Mock::given(method("GET"))
        .and(path("/next"))
        .respond_with(move |_: &wiremock::Request| {
            if fail_flag.load(Ordering::SeqCst) {
                failure_count.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            } else {
                success_count.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_bytes(MEDIA)
            }
        })
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let key = sync_token_key(ZONE);
    let mut original = pair(
        "selected-original",
        "original.JPG",
        &format!("{}/original", server.uri()),
        21,
    );
    let mut next = pair(
        "selected-next",
        "next.JPG",
        &format!("{}/next", server.uri()),
        22,
    );
    for records in [&mut original, &mut next] {
        records[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
    }
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut old_mtime = None;
    let mut next_mtime = None;
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let cancel = CancellationToken::new();
        let hit = Arc::new(AtomicBool::new(false));
        if cycle == 1 && fault == Fault::Checkpoint {
            let sql = format!("CREATE TRIGGER reject_current_checkpoint BEFORE INSERT ON metadata
                WHEN NEW.key='{}' BEGIN SELECT RAISE(ABORT,'synthetic v27 checkpoint failure'); END;",key);
            db.acquire_lock("install checkpoint fault")
                .unwrap()
                .execute_batch(&sql)
                .unwrap();
        }
        if cycle == 1 && fault == Fault::DownloadedStatus {
            db.acquire_lock("install downloaded status fault").unwrap().execute_batch(
                "CREATE TRIGGER reject_downloaded_status BEFORE UPDATE ON assets WHEN NEW.status='downloaded' AND NEW.id='asset-selected-next' BEGIN SELECT RAISE(ABORT,'synthetic downloaded status failure'); END;"
            ).unwrap();
        }
        if cycle == 2 {
            db.acquire_lock("remove checkpoint fault")
                .unwrap()
                .execute_batch("DROP TRIGGER IF EXISTS reject_current_checkpoint; DROP TRIGGER IF EXISTS reject_downloaded_status;")
                .unwrap();
            if matches!(fault, Fault::DeferredTransfer | Fault::DeferredSelection) {
                db.make_selection_retry_due(baseline_owner(), "asset-selected-next".into())
                    .await
                    .unwrap();
            }
        }
        fail_transfer.store(
            cycle == 1 && fault == Fault::DeferredTransfer,
            Ordering::SeqCst,
        );
        let inner = Session {
            // Deferred work must recover without another provider observation.
            cycle: if cycle == 0 {
                0
            } else if cycle == 1 || (cycle == 2 && fault != Fault::DeferredTransfer) {
                1
            } else {
                2
            },
            selection: Selection::Hidden,
            incomplete_window: false,
            original: Arc::new(original.clone()),
            next: Arc::new(next.clone()),
            queries: Arc::new(AtomicUsize::new(0)),
        };
        let session = InterruptedSession {
            inner: inner.clone(),
            fault: if cycle == 1 { Some(fault) } else { None },
            cancel: cancel.clone(),
            published: root.join("new-selected/next.JPG"),
            hit: hit.clone(),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let album = library.albums().await.unwrap().remove("Hidden").unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(
            ZONE,
            &key,
            vec![AlbumPass {
                kind: PassKind::SmartFolder,
                album,
                exclude_ids: Arc::default(),
            }],
        );
        selected.library = library;
        selected.plan_is_stale = cycle == 1 && fault == Fault::StalePlan;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.retry.max_retries = 0;
            config.folder_structure = "none".into();
            config.folder_structure_smart_folders = if cycle == 0 {
                "selected"
            } else {
                "new-selected"
            }
            .into();
            config.folder_structure_albums = config.folder_structure_smart_folders.clone();
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
            &cancel,
        )
        .await;

        let old = root.join("selected/original.JPG");
        assert_eq!(
            std::fs::read(&old).unwrap(),
            MEDIA,
            "{fault:?} cycle {cycle}"
        );
        let mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        if cycle == 0 {
            old_mtime = Some(mtime);
        }
        assert_eq!(Some(mtime), old_mtime);
        assert!(!root.join("new-selected/original.JPG").exists());
        if cycle == 1 {
            if fault == Fault::DownloadedStatus {
                assert!(
                    format!("{:?} {result:?}", trace.events())
                        .contains("synthetic downloaded status failure"),
                    "downloaded-status trigger must execute"
                );
            }
            if fault == Fault::Checkpoint {
                assert!(
                    format!("{:?} {result:?}", trace.events())
                        .contains("synthetic v27 checkpoint failure"),
                    "checkpoint trigger must execute"
                );
            }
            if fault == Fault::DeferredTransfer {
                assert!(
                    result.as_ref().unwrap().failed_count > 0,
                    "failed transfer must be visible"
                );
            }
            match &result {
                Ok(result) => assert!(
                    !result.can_advance_database_checkpoint(),
                    "{fault:?}: {result:?}"
                ),
                Err(error) => assert!(
                    fault == Fault::Checkpoint
                        && format!("{error:#}").contains("synthetic v27 checkpoint failure"),
                    "{error:#}"
                ),
            }
            if fault != Fault::DeferredTransfer {
                assert_eq!(
                    db.get_metadata(&key).await.unwrap().as_deref(),
                    Some("selected-after-1")
                );
            }
            if matches!(
                fault,
                Fault::EarlyCancel | Fault::LateCancel | Fault::MalformedPage
            ) {
                assert!(hit.load(Ordering::SeqCst), "fault boundary was not reached");
            }
            if matches!(
                fault,
                Fault::LateCancel
                    | Fault::MalformedPage
                    | Fault::Checkpoint
                    | Fault::DownloadedStatus
            ) {
                assert_eq!(
                    std::fs::read(root.join("new-selected/next.JPG")).unwrap(),
                    MEDIA
                );
            }
            if fault == Fault::DeferredSelection {
                assert!(
                    hit.load(Ordering::SeqCst),
                    "current confirmation must be attempted"
                );
                // Format two cannot losslessly admit this unknown identity.
                // The prior provider cursor, rather than a fabricated manifest,
                // is its durable replay obligation.
                assert_eq!(
                    db.get_metadata(&key).await.unwrap().as_deref(),
                    Some("selected-after-1")
                );
                assert!(!root.join("new-selected/next.JPG").exists());
            }
            if fault == Fault::DeferredTransfer {
                let count:i64=db.acquire_lock("prove pending destination").unwrap().query_row(
                    "SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND asset_id='asset-selected-next' AND admitted=1 AND verified_media=0",
                    [ZONE],|r|r.get(0)).unwrap();
                assert!(
                    count > 0,
                    "failed transfer must remain independently retryable"
                );
            }
        } else {
            let result = result.unwrap();
            assert_eq!(
                result.failed_count, 0,
                "{fault:?} cycle {cycle}: {result:?}"
            );
            assert!(
                result.can_advance_database_checkpoint(),
                "{fault:?} cycle {cycle}: {result:?}"
            );
            if cycle == 2 && fault == Fault::DeferredTransfer {
                assert_eq!(
                    inner.queries.load(Ordering::SeqCst),
                    0,
                    "independent retry must not enumerate inventory"
                );
                let pending:i64=db.acquire_lock("prove resolved decision").unwrap().query_row(
                    "SELECT count(*) FROM provider_active_destinations WHERE library='PrimarySync' AND asset_id='asset-selected-next' AND admitted=1 AND verified_media=0",
                    [],|r|r.get(0)).unwrap();
                assert_eq!(
                    pending, 0,
                    "admitted unfinished owner must resolve, not merely download via another owner"
                );
            }
            if cycle >= 2 {
                assert_eq!(
                    std::fs::read(root.join("new-selected/next.JPG")).unwrap(),
                    MEDIA
                );
            }
            if cycle == 3 {
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(
                    inner.queries.load(Ordering::SeqCst),
                    0,
                    "quiet reopen repeated inventory"
                );
            }
        }
        let next_path = root.join("new-selected/next.JPG");
        if next_path.exists() {
            let mtime = std::fs::metadata(&next_path).unwrap().modified().unwrap();
            if let Some(prior) = next_mtime {
                assert_eq!(mtime, prior);
            } else {
                next_mtime = Some(mtime);
            }
        }
    }
    assert_eq!(
        successful_next.load(Ordering::SeqCst),
        1,
        "new selected content transfers successfully exactly once"
    );
    if fault == Fault::DeferredTransfer {
        assert!(failed_next.load(Ordering::SeqCst) > 0);
    }
    server.verify().await;
}
