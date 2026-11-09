//! H1 narrow normal-cycle proof: a real owned baseline, one ordinary delta that
//! retains a current asset-only sparse identity while a peer asset completes,
//! then the real cycle -> report -> health path. The positive assertion is the
//! intended H1 behavior and intentionally fails on the pre-H A base.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::{SparseIdentityStore, SqliteStateDb};
use crate::sync_cycle::{
    CycleResult, ExpectedSource, SkipAuthority, run_cycle, run_cycle_with_witness, sync_token_key,
};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};

use super::{MEDIA, ZONE, baseline_owner, pair, zone};

const PEER_ZONE: &str = "SharedSync-v027-peer";

fn zone_for(name: &str) -> Value {
    json!({"zoneName": name, "ownerRecordName": "_defaultOwner"})
}

#[derive(Clone)]
struct Session {
    baseline: Arc<Vec<Value>>,
    progress: Arc<Vec<Value>>,
    hold_child: Arc<Value>,
    zone_name: String,
    empty: bool,
    cycle: usize,
    fail_inventory: bool,
    provider_error: bool,
    provider_errors: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(json!({"zones":[{"zoneID":zone_for(&self.zone_name),
                "syncToken":"op-hold-boundary","deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"recordName":"index",
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(if self.empty { 0 } else { 2 }));
        }
        if url.contains("/records/query?") {
            if self.fail_inventory && self.cycle > 0 {
                return Ok(json!({"records":"malformed inventory page"}));
            }
            if self.empty {
                return Ok(json!({"records":[],
                    "syncToken":"op-query-not-checkpoint"}));
            }
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.baseline.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"op-query-not-checkpoint"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let mut records = Vec::new();
            for request in requested {
                let name = request["recordName"].as_str().unwrap_or_default();
                if name == "op-hold-child" {
                    // Asset-only child with no masterRef and no provider answer.
                    continue;
                }
                records.extend(
                    self.progress
                        .iter()
                        .chain(self.baseline.iter())
                        .filter(|candidate| candidate["recordName"].as_str() == Some(name))
                        .cloned(),
                );
            }
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            self.changes.fetch_add(1, Ordering::SeqCst);
            if self.provider_error && self.cycle > 0 {
                self.provider_errors.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"zones":[{"zoneID":zone_for(&self.zone_name),
                    "serverErrorCode":"BAD_REQUEST",
                    "reason":"synthetic expired retained cursor",
                    "syncToken":"","moreComing":false}]}));
            }
            let token = request["zones"][0]["syncToken"]
                .as_str()
                .unwrap_or_default();
            let (records, successor) = if self.cycle == 0 && token == "op-hold-boundary" {
                (Vec::new(), "op-hold-after-baseline".to_string())
            } else if self.cycle == 0 {
                assert_eq!(
                    token, "op-hold-after-baseline",
                    "baseline replay must use the saved boundary"
                );
                (Vec::new(), "op-hold-after-baseline".to_string())
            } else {
                assert_eq!(
                    token, "op-hold-after-baseline",
                    "a held cycle must retain the baseline cursor"
                );
                let mut records = self.progress.as_ref().clone();
                records.push(self.hold_child.as_ref().clone());
                (records, "op-hold-after-hold".to_string())
            };
            return Ok(json!({"zones":[{"zoneID":zone_for(&self.zone_name),
                "records":records,"syncToken":successor,"moreComing":false}]}));
        }
        anyhow::bail!("unexpected operational hold endpoint: {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: Arc<SqliteStateDb>,
    shared: crate::auth::session::SharedSession,
    config: crate::config::Config,
    baseline: Arc<Vec<Value>>,
    progress: Arc<Vec<Value>>,
    hold_child: Arc<Value>,
    key: String,
    peer_errors: Arc<AtomicUsize>,
    _server: wiremock::MockServer,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let server = crate::test_helpers::start_wiremock_or_skip("operational_holds").await?;
        for name in ["baseline", "progress"] {
            Mock::given(method("GET"))
                .and(path(format!("/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
                .expect(1)
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = dir.path().join("state.db");
        let baseline = Arc::new(pair(
            "op-baseline",
            "baseline.JPG",
            &format!("{}/baseline", server.uri()),
            41,
        ));
        let progress = Arc::new(pair(
            "op-progress",
            "progress.JPG",
            &format!("{}/progress", server.uri()),
            42,
        ));
        let mut hold_child = pair(
            "op-hold-child",
            "hold.JPG",
            &format!("{}/hold", server.uri()),
            43,
        )[1]
        .clone();
        hold_child["fields"]
            .as_object_mut()
            .unwrap()
            .remove("masterRef");
        let hold_child = Arc::new(hold_child);
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let config = make_run_cycle_config();
        let key = sync_token_key(ZONE);
        Some(Self {
            _dir: dir,
            root,
            db,
            shared,
            config,
            baseline,
            progress,
            hold_child,
            key,
            peer_errors: Arc::new(AtomicUsize::new(0)),
            _server: server,
        })
    }

    async fn run(&self, cycle: usize, fail_inventory: bool, provider_error: bool) -> CycleResult {
        self.run_with_expected_sources(cycle, fail_inventory, provider_error, &[])
            .await
    }

    async fn run_with_expected_sources(
        &self,
        cycle: usize,
        fail_inventory: bool,
        provider_error: bool,
        extra_sources: &[ExpectedSource],
    ) -> CycleResult {
        let session = Session {
            baseline: Arc::clone(&self.baseline),
            progress: Arc::clone(&self.progress),
            hold_child: Arc::clone(&self.hold_child),
            zone_name: ZONE.to_owned(),
            empty: false,
            cycle,
            fail_inventory,
            provider_error,
            provider_errors: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            Arc::clone(&self.db),
            baseline_owner(),
            "com",
        ));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        };
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &self.key, vec![pass]);
        selected.library = library;
        let db: Arc<dyn download::DownloadStore> = self.db.clone();
        let base = make_run_cycle_download_config_builder(&self.root, db);
        let builder = |mode, exclude, groups, zone| {
            let mut download = (*base(mode, exclude, groups, zone)).clone();
            download.folder_structure = "none".into();
            download.retry.max_retries = 0;
            Arc::new(download)
        };
        let mut expected_sources = vec![ExpectedSource {
            zone_name: ZONE.to_owned(),
            baseline_scope: selected.library.baseline_scope(),
            active_passes: !selected.plan.passes.is_empty(),
            skip_authority: SkipAuthority::Reached,
        }];
        expected_sources.extend_from_slice(extra_sources);
        run_cycle_with_witness(
            &[&selected],
            &self.config,
            Some(self.db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &self.shared,
            &CancellationToken::new(),
            Arc::new(download::MetadataWriterWitness::default()),
            &expected_sources,
        )
        .await
        .unwrap()
    }

    async fn library_state(
        &self,
        session: Session,
        zone_name: &str,
        key: &str,
    ) -> crate::sync_cycle::LibraryState {
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session),
            Arc::new(zone_for(zone_name)),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            Arc::clone(&self.db),
            baseline_owner(),
            "com",
        ));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        };
        let mut selected = make_run_cycle_library_state_with_passes(zone_name, key, vec![pass]);
        selected.library = library;
        selected
    }

    async fn run_two(&self, cycle: usize, provider_error: bool, peer_first: bool) -> CycleResult {
        self.peer_errors.store(0, Ordering::SeqCst);
        let primary = self
            .library_state(
                Session {
                    baseline: Arc::clone(&self.baseline),
                    progress: Arc::clone(&self.progress),
                    hold_child: Arc::clone(&self.hold_child),
                    zone_name: ZONE.to_owned(),
                    empty: false,
                    cycle,
                    fail_inventory: false,
                    provider_error: false,
                    provider_errors: Arc::new(AtomicUsize::new(0)),
                    changes: Arc::new(AtomicUsize::new(0)),
                },
                ZONE,
                &self.key,
            )
            .await;
        let peer_key = sync_token_key(PEER_ZONE);
        let peer = self
            .library_state(
                Session {
                    baseline: Arc::clone(&self.baseline),
                    progress: Arc::clone(&self.progress),
                    hold_child: Arc::clone(&self.hold_child),
                    zone_name: PEER_ZONE.to_owned(),
                    empty: true,
                    cycle,
                    fail_inventory: false,
                    provider_error,
                    provider_errors: Arc::clone(&self.peer_errors),
                    changes: Arc::new(AtomicUsize::new(0)),
                },
                PEER_ZONE,
                &peer_key,
            )
            .await;
        let db: Arc<dyn download::DownloadStore> = self.db.clone();
        let base = make_run_cycle_download_config_builder(&self.root, db);
        let builder = |mode, exclude, groups, zone| {
            let mut download = (*base(mode, exclude, groups, zone)).clone();
            download.folder_structure = "none".into();
            download.retry.max_retries = 0;
            Arc::new(download)
        };
        let states = if peer_first {
            vec![&peer, &primary]
        } else {
            vec![&primary, &peer]
        };
        run_cycle(
            &states,
            &self.config,
            Some(self.db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &self.shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    }
}

fn cycle_facts(result: &CycleResult) -> String {
    format!(
        "failed_count={} stats.failed={} held={} blocked={:?}",
        result.failed_count,
        result.stats.failed,
        !result.holds.is_empty(),
        result.stats.sync_token_blocked_reason
    )
}

async fn reported_health(
    fixture: &Fixture,
    result: &CycleResult,
    name: &str,
) -> (crate::health::HealthStatus, Value) {
    let report_path = fixture.root.join(name);
    let notifier = crate::notifications::Notifier::new(None);
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&fixture.config),
            health_dir: fixture.root.as_path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(fixture.db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(
                result,
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(&report_path).expect("reported health report"),
    )
    .expect("reported health report json");
    (health, report)
}

async fn report_health(fixture: &Fixture, result: &CycleResult) {
    let report_path = fixture.root.join("hold-report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&fixture.config),
            health_dir: fixture.root.as_path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(fixture.db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(
                result,
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).expect("hold report"))
            .expect("hold report json");
    assert_eq!(report["status"], "partial_failure", "{report}");
    assert_eq!(report["stats"]["failed"], 0, "{report}");
    assert!(
        report["stats"]["sync_token_blocked_zone"] == ZONE,
        "the exact source remains visible: {report}"
    );
    let db = &fixture.db;
    assert_eq!(
        db.get_metadata(&fixture.key).await.unwrap().as_deref(),
        Some("op-hold-after-baseline"),
        "the held cycle must retain the baseline cursor"
    );
    let row = db.sparse_identities(ZONE).await.unwrap().remove(0);
    assert!(
        row.next_retry.is_none(),
        "a due/NULL retry remains legitimate"
    );
    assert!(
        row.origin
            .as_deref()
            .is_some_and(|origin| origin.contains("current-v1")),
        "the retained row must carry the current owned scope"
    );
    let conn = db.acquire_lock("inspect operational hold row").unwrap();
    let (source, generation, attempts, next_retry): (String, i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT source_record_name,generation,attempts,next_retry_at \
             FROM unresolved_sparse_identities WHERE library=?1",
            [ZONE],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert!(
        source.contains("hold"),
        "the retained row must be the observed child: {source}"
    );
    assert!(generation > 0, "the current row must retain a generation");
    assert_eq!(attempts, 0, "the initial due retry keeps zero attempts");
    assert_eq!(
        next_retry, None,
        "the initial due retry has no saved deadline"
    );
    assert!(
        !fixture.root.join("hold.JPG").exists(),
        "an unresolved child must not be guessed or downloaded"
    );
    assert!(
        fixture.root.join("progress.JPG").exists(),
        "the peer progress asset must complete"
    );
    health_success_assertion(&health);
}

fn health_success_assertion(health: &crate::health::HealthStatus) {
    // Intended H1 behavior. This is the expected pre-repair failure on the A
    // base: the safe hold currently records a health failure.
    assert!(
        health.last_success_at.is_some(),
        "the safe hold must advance the operational success clock"
    );
    assert_eq!(
        health.consecutive_failures, 0,
        "the safe hold must not consume the genuine-failure threshold"
    );
}

#[tokio::test]
async fn operational_safe_current_sparse_hold_advances_health_only_after_h1() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let baseline = fixture.run(0, false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{}", cycle_facts(&baseline));
    assert!(
        baseline.can_advance_database_checkpoint(),
        "{}",
        cycle_facts(&baseline)
    );
    let held = fixture.run(1, false, false).await;
    assert!(
        !held.can_advance_database_checkpoint(),
        "{}",
        cycle_facts(&held)
    );
    assert_eq!(
        held.stats.failed,
        0,
        "no transfer failure is allowed: {}",
        cycle_facts(&held)
    );
    assert_eq!(
        held.stats.downloaded,
        1,
        "peer progress must download: {}",
        cycle_facts(&held)
    );
    assert!(
        held.holds.iter().any(|hold| {
            hold.source == ZONE
                && hold.kind == crate::sync_cycle::LibraryHoldKind::IdentityUnresolved
        }),
        "one exact current identity hold must be visible: {}",
        cycle_facts(&held)
    );
    report_health(&fixture, &held).await;
}

#[tokio::test]
async fn operational_safe_hold_refuses_a_mixed_state_fault() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let baseline = fixture.run(0, false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{}", cycle_facts(&baseline));
    fixture
        .db
        .acquire_lock("install hold checkpoint fault")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_operational_hold BEFORE INSERT ON metadata \
             WHEN NEW.key='last_checkpoint_status' \
             BEGIN SELECT RAISE(ABORT,'synthetic operational state fault'); END;",
        )
        .unwrap();
    let held = fixture.run(1, false, false).await;
    assert!(
        !held.can_advance_database_checkpoint(),
        "{}",
        cycle_facts(&held)
    );
    let report_path = fixture.root.join("mixed-hold-report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&fixture.config),
            health_dir: fixture.root.as_path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(fixture.db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(
                &held,
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    assert!(
        health.last_success_at.is_none(),
        "a mixed state fault must not advance health"
    );
    assert!(health.consecutive_failures > 0);
}

#[tokio::test]
async fn operational_safe_hold_refuses_real_provider_bad_request() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let baseline = fixture.run(0, false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{}", cycle_facts(&baseline));
    let safe_hold = fixture.run(1, false, false).await;
    assert!(
        !safe_hold.can_advance_database_checkpoint(),
        "{}",
        cycle_facts(&safe_hold)
    );
    let held = fixture.run(2, false, true).await;
    assert!(
        held.stats.enumeration_errors == 0,
        "the baseline counter can miss the real provider error: {}",
        cycle_facts(&held)
    );
    assert!(
        !held.can_advance_database_checkpoint(),
        "{}",
        cycle_facts(&held)
    );
    assert!(
        held.failed_count >= 1
            || held
                .stats
                .sync_token_blocked_reason
                .is_some_and(|reason| reason != "asset_delta_hydration_incomplete"),
        "the real provider error must remain in the cycle failure facts: {}",
        cycle_facts(&held)
    );
    assert!(
        held.holds.iter().any(|hold| {
            hold.source == ZONE
                && hold.kind != crate::sync_cycle::LibraryHoldKind::IdentityUnresolved
        }),
        "the provider error adds a genuine non-sparse hold: {}",
        cycle_facts(&held)
    );
    let report_path = fixture.root.join("provider-error-report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&fixture.config),
            health_dir: fixture.root.as_path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(fixture.db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(
                &held,
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    assert!(
        health.last_success_at.is_none(),
        "a real provider error must veto the safe hold even when enum counters are zero"
    );
    assert!(health.consecutive_failures > 0);
}

#[tokio::test]
async fn operational_safe_hold_refuses_peer_bad_request_primary_first() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let baseline = fixture.run_two(0, false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{}", cycle_facts(&baseline));
    let held = fixture.run_two(1, true, false).await;
    assert_eq!(held.stats.enumeration_errors, 0, "{}", cycle_facts(&held));
    assert!(
        !held.can_advance_database_checkpoint(),
        "{}",
        cycle_facts(&held)
    );
    assert!(
        held.holds.iter().any(|hold| {
            hold.source == ZONE
                && hold.kind == crate::sync_cycle::LibraryHoldKind::IdentityUnresolved
        }),
        "the Primary safe sparse hold must remain: {}",
        cycle_facts(&held)
    );
    assert!(
        fixture.peer_errors.load(Ordering::SeqCst) > 0,
        "the peer real BAD_REQUEST must actually execute: {}",
        cycle_facts(&held)
    );
    assert_eq!(
        held.stats.sync_token_blocked_zone.as_deref(),
        Some(ZONE),
        "the first aggregate blocked reason must stay Primary: {}",
        cycle_facts(&held)
    );
    assert_eq!(
        held.stats.sync_token_blocked_reason,
        Some("asset_delta_hydration_incomplete"),
        "the first aggregate blocked reason must stay the Primary sparse reason: {}",
        cycle_facts(&held)
    );
    let (health, report) = reported_health(&fixture, &held, "peer-first-report.json").await;
    assert_eq!(report["status"], "partial_failure", "{report}");
    assert_eq!(report["stats"]["failed"], 0, "{report}");
    assert!(
        health.last_success_at.is_none(),
        "a peer provider error must veto the Primary safe hold"
    );
    assert!(health.consecutive_failures > 0);
}

#[tokio::test]
async fn operational_safe_hold_refuses_peer_bad_request_peer_first() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let baseline = fixture.run_two(0, false, true).await;
    assert_eq!(baseline.stats.downloaded, 1, "{}", cycle_facts(&baseline));
    let held = fixture.run_two(1, true, true).await;
    assert_eq!(held.stats.enumeration_errors, 0, "{}", cycle_facts(&held));
    assert!(
        !held.can_advance_database_checkpoint(),
        "{}",
        cycle_facts(&held)
    );
    assert!(
        held.holds.iter().any(|hold| {
            hold.source == ZONE
                && hold.kind == crate::sync_cycle::LibraryHoldKind::IdentityUnresolved
        }),
        "the Primary safe sparse hold must remain: {}",
        cycle_facts(&held)
    );
    assert!(
        fixture.peer_errors.load(Ordering::SeqCst) > 0,
        "the peer real BAD_REQUEST must actually execute: {}",
        cycle_facts(&held)
    );
    let (health, report) = reported_health(&fixture, &held, "peer-second-report.json").await;
    assert_eq!(report["status"], "partial_failure", "{report}");
    assert_eq!(report["stats"]["failed"], 0, "{report}");
    assert!(
        health.last_success_at.is_none(),
        "the peer error must veto the Primary safe hold in either source order"
    );
    assert!(health.consecutive_failures > 0);
}

async fn report_health_expect_failure(fixture: &Fixture, result: &CycleResult) {
    let report_path = fixture.root.join("hold-refusal-report.json");
    let notifier = crate::notifications::Notifier::new(None);
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: false,
            report_path: Some(&report_path),
            run_options: crate::report::RunOptions::from_config(&fixture.config),
            health_dir: fixture.root.as_path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(fixture.db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(
                result,
                std::time::Duration::from_secs(1),
            ),
        )
        .await;
    assert!(health.last_success_at.is_none());
    assert!(health.consecutive_failures > 0);
}

#[tokio::test]
async fn operational_safe_hold_refuses_unvisited_active_source() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let baseline = fixture.run(0, false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{}", cycle_facts(&baseline));
    let held = fixture
        .run_with_expected_sources(
            1,
            false,
            false,
            &[ExpectedSource {
                zone_name: "SharedSync-unvisited".to_owned(),
                baseline_scope: "unvisited-scope".to_owned(),
                active_passes: true,
                skip_authority: SkipAuthority::Reached,
            }],
        )
        .await;
    assert!(
        !held.operational_safe_holds,
        "an unvisited active source must veto even beside a safe hold: {}",
        cycle_facts(&held)
    );
    report_health_expect_failure(&fixture, &held).await;
}

#[tokio::test]
async fn operational_safe_hold_allows_authorized_no_changes_peer() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let baseline = fixture.run(0, false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{}", cycle_facts(&baseline));
    let held = fixture
        .run_with_expected_sources(
            1,
            false,
            false,
            &[ExpectedSource {
                zone_name: "SharedSync-no-changes".to_owned(),
                baseline_scope: "no-changes-scope".to_owned(),
                active_passes: true,
                skip_authority: SkipAuthority::NoChanges,
            }],
        )
        .await;
    assert!(
        held.operational_safe_holds,
        "same-wake successful no-changes authority may be skipped: {}",
        cycle_facts(&held)
    );
    report_health(&fixture, &held).await;
}
