//! A retained-source refusal is local to its library, including after reopen.
use super::super::{MEDIA, ZONE, baseline_owner, pair, zone};
use super::{captured, damage};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::state::db::provider_inbox::CapturedPageId;
use crate::state::db::provider_work::{MAX_WORK_BYTES, WorkAdmission, WorkPlan, WorkSource};
use crate::sync_cycle::{LibraryState, run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path},
};

const HEALTHY: &str = "PrimarySync-v027-healthy";
fn healthy_zone() -> Value {
    json!({"zoneName":HEALTHY,"ownerRecordName":"_defaultOwner"})
}

#[derive(Clone)]
struct HealthySession {
    cycle: usize,
    records: Arc<Vec<Value>>,
    inventory: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
    boundaries: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for HealthySession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            self.boundaries.fetch_add(1, Ordering::SeqCst);
            return Ok(
                json!({"zones":[{"zoneID":healthy_zone(),"syncToken":"healthy-boundary","deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(1));
        }
        if url.contains("/records/query?") {
            assert_eq!(request["zoneID"], healthy_zone());
            self.inventory.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset==0 {self.records.as_ref().clone()} else {vec![]},
                "syncToken":"healthy-query"}),
            );
        }
        if url.contains("/changes/zone?") {
            self.changes.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request["zones"][0]["zoneID"], healthy_zone());
            assert_eq!(
                request["zones"][0]["syncToken"],
                if self.cycle == 0 {
                    "healthy-boundary".into()
                } else {
                    format!("healthy-cycle-{}", self.cycle)
                }
            );
            return Ok(json!({"zones":[{"zoneID":healthy_zone(),"records":[],
                "syncToken":format!("healthy-cycle-{}",self.cycle+1),"moreComing":false}]}));
        }
        if url.contains("/records/lookup?") {
            assert_eq!(request["zoneID"], healthy_zone());
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|r| r["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        anyhow::bail!("unexpected healthy fixture request")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct BlockedSession;
#[async_trait::async_trait]
impl PhotosSession for BlockedSession {
    async fn post(&self, _: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        anyhow::bail!("blocked library must stop before provider enumeration or hydration")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn selected(
    db: Arc<SqliteStateDb>,
    zone: Value,
    session: Box<dyn PhotosSession>,
) -> LibraryState {
    let name = zone["zoneName"].as_str().unwrap().to_owned();
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        session,
        Arc::new(zone),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(db, baseline_owner(), "com"));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut state = make_run_cycle_library_state_with_passes(&name, &sync_token_key(&name), passes);
    state.library = library;
    state
}

type RetainedSource = (Vec<u8>, String, i64, i64, i64);
fn snapshot(db: &SqliteStateDb, id: i64) -> RetainedSource {
    db.acquire_lock("blocked source unchanged")
        .unwrap()
        .query_row(
            "SELECT p.body,r.config_hash,r.attempts,r.last_attempt_at,r.next_retry_at
         FROM provider_work_retries r JOIN provider_shadow_pages p ON p.id=r.page_id WHERE p.id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn v027_blocked_and_healthy_libraries_share_cycle_and_reopen_in_either_order() {
    for blocked_first in [true, false] {
        prove_isolation(blocked_first).await;
    }
}

async fn prove_isolation(blocked_first: bool) {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/healthy"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(MEDIA)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let sentinel = root.join("existing.bin");
    std::fs::write(&sentinel, b"existing archive bytes").unwrap();
    let sentinel_mtime = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    let database = dir.path().join("state.db");
    let mut records = pair(
        "healthy",
        "healthy.JPG",
        &format!("{}/healthy", server.uri()),
        51,
    );
    records[1]["fields"]["masterRef"]["value"]["zoneID"] = healthy_zone();
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut retained = None;
    let mut publication = None;
    let mut source_id = 0;

    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let blocked = selected(db.clone(), zone(), Box::new(BlockedSession)).await;
        let session = HealthySession {
            cycle,
            records: Arc::new(records.clone()),
            inventory: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
        };
        let healthy = selected(db.clone(), healthy_zone(), Box::new(session.clone())).await;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, excludes, groups, name| {
            let mut config = (*base(mode, excludes, groups, name)).clone();
            config.folder_structure = "none".into();
            config.metadata.xmp_sidecar = false;
            Arc::new(config)
        };
        if cycle == 0 {
            let capture = blocked.library.shadow_capture.as_ref().unwrap();
            let (id, ordinal) = captured(capture).await;
            source_id = id;
            let seed_config = builder(
                download::SyncMode::Full,
                Arc::new(rustc_hash::FxHashSet::default()),
                Arc::new(download::AssetGroupings::default()),
                Arc::from(ZONE),
            );
            let effective = seed_config.with_pass(&blocked.plan.passes[0]);
            let hash = download::retained_work_config_hash_for_test(&effective, &zone());
            let source = db
                .catalog_source(baseline_owner(), CapturedPageId(id), 16 * 1024 * 1024)
                .await
                .unwrap();
            assert_eq!(
                db.project_provider_work(
                    baseline_owner(),
                    WorkPlan {
                        source: WorkSource {
                            page: source,
                            ordinal
                        },
                        scope: capture.scope("private", &zone()).unwrap(),
                        zone: zone(),
                        config_hash: hash,
                        confirmation: None,
                        master: None,
                        records: vec![],
                        reason: "current_lookup_unresolved",
                    },
                    MAX_WORK_BYTES
                )
                .await
                .unwrap(),
                WorkAdmission::Deferred
            );
            db.acquire_lock("due historical dependency")
                .unwrap()
                .execute_batch("UPDATE provider_work_retries SET last_attempt_at=0,next_retry_at=1")
                .unwrap();
            db.set_metadata(&sync_token_key(ZONE), "blocked-current-cursor")
                .await
                .unwrap();
            damage(&db, id);
            retained = Some(snapshot(&db, id));
        }
        let libraries = if blocked_first {
            vec![&blocked, &healthy]
        } else {
            vec![&healthy, &blocked]
        };
        let result = run_cycle(
            &libraries,
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
        assert_eq!(
            result.failed_count, 1,
            "cycle={cycle} blocked_first={blocked_first}: {result:?}"
        );
        assert!(
            !result.db_sync_token_advance_safe,
            "database-wide cursor remains conservative"
        );
        assert_eq!(result.stats.sync_token_blocked_zone.as_deref(), Some(ZONE));
        assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
        assert_eq!(
            db.get_metadata(&sync_token_key(ZONE))
                .await
                .unwrap()
                .as_deref(),
            Some("blocked-current-cursor")
        );
        assert_eq!(
            db.get_metadata(&sync_token_key(HEALTHY)).await.unwrap(),
            Some(format!("healthy-cycle-{}", cycle + 1))
        );
        assert_eq!(snapshot(&db, source_id), *retained.as_ref().unwrap());
        assert_eq!(session.changes.load(Ordering::SeqCst), 1);
        if cycle > 0 {
            assert_eq!(session.inventory.load(Ordering::SeqCst), 0);
            assert_eq!(session.boundaries.load(Ordering::SeqCst), 0);
        }
        let bytes = std::fs::read(root.join("healthy.JPG")).unwrap();
        assert_eq!(bytes, MEDIA);
        let mtime = std::fs::metadata(root.join("healthy.JPG"))
            .unwrap()
            .modified()
            .unwrap();
        if let Some((prior_bytes, prior_mtime)) = &publication {
            assert_eq!(&bytes, prior_bytes);
            assert_eq!(&mtime, prior_mtime);
        }
        publication = Some((bytes, mtime));
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"existing archive bytes");
        assert_eq!(
            std::fs::metadata(&sentinel).unwrap().modified().unwrap(),
            sentinel_mtime
        );
    }
    server.verify().await;
}
