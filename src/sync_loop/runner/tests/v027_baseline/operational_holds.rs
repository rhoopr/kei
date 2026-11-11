//! H1 narrow normal-cycle proof: a real owned baseline, one ordinary delta that
//! retains a current asset-only sparse identity while a peer asset completes,
//! then the real cycle -> report -> health path. The positive assertion is the
//! intended H1 behavior and intentionally fails on the pre-H A base.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
    make_run_cycle_library_state_with_passes,
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
    hold_pair: Arc<Vec<Value>>,
    followup_child: Arc<Value>,
    recovery_events: Arc<Mutex<Vec<String>>>,
    zone_name: String,
    empty: bool,
    cycle: usize,
    fail_inventory: bool,
    provider_error: bool,
    provider_errors: Arc<AtomicUsize>,
    changes: Arc<AtomicUsize>,
    inventory_queries: Arc<AtomicUsize>,
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
            if self.cycle == 5 {
                self.recovery_events.lock().unwrap().push("boundary".into());
            }
            return Ok(json!({"zones":[{"zoneID":zone_for(&self.zone_name),
                "syncToken":match self.cycle { 3 => "pair-fresh-one", 5 => "pair-fresh-two", _ => "op-hold-boundary" },"deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"recordName":"index",
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(if self.empty {
                0
            } else if self.cycle == 5 {
                4
            } else {
                2
            }));
        }
        if url.contains("/records/query?") {
            self.inventory_queries.fetch_add(1, Ordering::SeqCst);
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
                let mut records = self.baseline.as_ref().clone();
                if self.cycle == 5 {
                    self.recovery_events
                        .lock()
                        .unwrap()
                        .push("inventory".into());
                    records.extend(self.progress.iter().cloned());
                    records.extend(self.hold_pair.iter().cloned());
                }
                records
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"op-query-not-checkpoint"}));
        }
        if url.contains("/records/lookup?") {
            if self.cycle == 5 {
                assert_eq!(request["zoneID"], zone_for(&self.zone_name));
            }
            let requested = request["records"].as_array().unwrap();
            let mut records = Vec::new();
            for request in requested {
                let name = request["recordName"].as_str().unwrap_or_default();
                if self.cycle == 5 {
                    self.recovery_events
                        .lock()
                        .unwrap()
                        .push(format!("lookup:{name}"));
                    records.extend(
                        self.hold_pair
                            .iter()
                            .filter(|record| record["recordName"].as_str() == Some(name))
                            .cloned(),
                    );
                }
                if name == "op-hold-child" {
                    // Only the recovery phase supplied this previously missing master.
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
            if self.cycle == 5 {
                for record in &mut records {
                    record["zoneID"] = zone_for(&self.zone_name);
                }
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
            if (self.cycle == 3 && token == "op-hold-after-baseline")
                || (self.cycle == 5 && token == "pair-after-one")
            {
                return Ok(json!({"zones":[{"zoneID":zone_for(&self.zone_name),
                    "serverErrorCode":"BAD_REQUEST"}]}));
            }
            let (records, successor) = if self.cycle == 0 && token == "op-hold-boundary" {
                (Vec::new(), "op-hold-after-baseline".to_string())
            } else if self.cycle == 0 {
                assert_eq!(
                    token, "op-hold-after-baseline",
                    "baseline replay must use the saved boundary"
                );
                (Vec::new(), "op-hold-after-baseline".to_string())
            } else if self.cycle == 3 && token == "pair-fresh-one" {
                (Vec::new(), "pair-after-one".to_owned())
            } else if self.cycle == 5 && token == "pair-fresh-two" {
                self.recovery_events.lock().unwrap().push("bridge".into());
                (Vec::new(), "pair-after-two".to_owned())
            } else if self.cycle == 6 {
                assert_eq!(token, "pair-after-two");
                (
                    vec![self.followup_child.as_ref().clone()],
                    "pair-after-three".to_owned(),
                )
            } else if self.cycle == 7 {
                assert_eq!(token, "op-hold-after-baseline");
                (self.progress.as_ref().clone(), "op-after-disk".to_owned())
            } else if self.cycle == 1 || self.cycle == 4 {
                assert_eq!(
                    token,
                    match self.cycle {
                        4 => "pair-after-one",
                        _ => "op-hold-after-baseline",
                    },
                    "a held cycle must retain the baseline cursor"
                );
                let mut records = self.progress.as_ref().clone();
                records.push(self.hold_child.as_ref().clone());
                (records, "op-hold-after-hold".to_string())
            } else {
                assert_eq!(token, "op-hold-after-baseline");
                (Vec::new(), "op-hold-after-quiet".to_string())
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

pub(super) struct Fixture {
    _dir: tempfile::TempDir,
    pub(super) root: PathBuf,
    pub(super) db: Arc<SqliteStateDb>,
    shared: crate::auth::session::SharedSession,
    config: crate::config::Config,
    baseline: Arc<Vec<Value>>,
    progress: Arc<Vec<Value>>,
    hold_child: Arc<Value>,
    hold_pair: Arc<Vec<Value>>,
    followup_child: Arc<Value>,
    pub(super) recovery_events: Arc<Mutex<Vec<String>>>,
    pub(super) key: String,
    peer_errors: Arc<AtomicUsize>,
    pub(super) _server: wiremock::MockServer,
    pub(super) inventory_queries: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        Self::with_progress_inner(
            |_| ResponseTemplate::new(200).set_body_bytes(MEDIA),
            true,
            None,
            1,
        )
        .await
    }

    pub(super) async fn with_progress<R: wiremock::Respond + Send + Sync + 'static>(
        response: impl FnOnce(PathBuf) -> R,
    ) -> Option<Self> {
        Self::with_progress_inner(response, false, None, 1).await
    }

    async fn with_progress_inner<R: wiremock::Respond + Send + Sync + 'static>(
        response: impl FnOnce(PathBuf) -> R,
        exact_one: bool,
        media_root: Option<&std::path::Path>,
        progress_requests: u64,
    ) -> Option<Self> {
        let server = crate::test_helpers::start_wiremock_or_skip("operational_holds").await?;
        let dir = tempfile::tempdir().unwrap();
        let root =
            media_root.map_or_else(|| dir.path().join("media"), std::path::Path::to_path_buf);
        std::fs::create_dir(&root).unwrap();
        let database = dir.path().join("state.db");
        Mock::given(method("GET"))
            .and(path("/baseline"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
        let progress = Mock::given(method("GET"))
            .and(path("/progress"))
            .respond_with(response(database.clone()));
        if exact_one {
            progress.expect(progress_requests).mount(&server).await;
        } else {
            progress.expect(1..).mount(&server).await;
        }
        let baseline = Arc::new(pair(
            "op-baseline",
            "baseline.JPG",
            &format!("{}/baseline", server.uri()),
            41,
        ));
        let mut progress = pair(
            "op-progress",
            "progress.JPG",
            &format!("{}/progress", server.uri()),
            42,
        );
        if !exact_one {
            Mock::given(method("GET"))
                .and(path("/healthy-peer"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
                .expect(1)
                .mount(&server)
                .await;
            progress.extend(pair(
                "op-healthy-peer",
                "healthy-peer.JPG",
                &format!("{}/healthy-peer", server.uri()),
                44,
            ));
        }
        let progress = Arc::new(progress);
        Mock::given(method("GET"))
            .and(path("/hold"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .mount(&server)
            .await;
        let hold_pair = Arc::new(pair(
            "op-hold-child",
            "hold.JPG",
            &format!("{}/hold", server.uri()),
            43,
        ));
        let mut hold_child = hold_pair[1].clone();
        hold_child["fields"]
            .as_object_mut()
            .unwrap()
            .remove("masterRef");
        let hold_child = Arc::new(hold_child);
        let mut followup_child = pair(
            "op-followup-hold",
            "followup.JPG",
            &format!("{}/followup", server.uri()),
            45,
        )[1]
        .clone();
        followup_child["fields"]
            .as_object_mut()
            .unwrap()
            .remove("masterRef");
        let followup_child = Arc::new(followup_child);
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        // Keep the actual authenticated account lock for the life of the
        // fixture, including the reopened cycle and final health observation.
        let mut session = crate::auth::session::Session::new(
            dir.path(),
            "baseline@example.invalid",
            "https://www.icloud.com",
            None,
        )
        .await
        .unwrap();
        session
            .bind_principal(
                &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-baseline"}})).unwrap(),
                None,
            )
            .unwrap();
        let shared = Arc::new(tokio::sync::RwLock::new(session));
        let mut config = make_run_cycle_config();
        config.auth.username = "baseline@example.invalid".to_owned();
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
            hold_pair,
            followup_child,
            recovery_events: Arc::new(Mutex::new(Vec::new())),
            key,
            peer_errors: Arc::new(AtomicUsize::new(0)),
            _server: server,
            inventory_queries: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(super) async fn reopen(self) -> Self {
        let Self {
            _dir,
            root,
            db,
            shared,
            config,
            baseline,
            progress,
            hold_child,
            hold_pair,
            followup_child,
            recovery_events,
            key,
            peer_errors,
            _server,
            inventory_queries,
        } = self;
        assert_eq!(
            Arc::strong_count(&db),
            1,
            "all cycle database handles must close"
        );
        drop(db);
        let db = Arc::new(
            SqliteStateDb::open_owned(&_dir.path().join("state.db"), &baseline_owner())
                .await
                .unwrap(),
        );
        Self {
            _dir,
            root,
            db,
            shared,
            config,
            baseline,
            progress,
            hold_child,
            hold_pair,
            followup_child,
            recovery_events,
            key,
            peer_errors,
            _server,
            inventory_queries,
        }
    }

    pub(super) async fn run(
        &self,
        cycle: usize,
        fail_inventory: bool,
        provider_error: bool,
    ) -> CycleResult {
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
            hold_pair: Arc::clone(&self.hold_pair),
            followup_child: Arc::clone(&self.followup_child),
            recovery_events: Arc::clone(&self.recovery_events),
            zone_name: ZONE.to_owned(),
            empty: false,
            cycle,
            fail_inventory,
            provider_error,
            provider_errors: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
            inventory_queries: Arc::clone(&self.inventory_queries),
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
            if cycle == 7 {
                download.temp_suffix = Arc::from(".part");
            }
            Arc::new(download)
        };
        let mut expected_sources = vec![ExpectedSource {
            zone_name: ZONE.to_owned(),
            baseline_scope: selected.library.baseline_scope(),
            active_passes: !selected.plan.passes.is_empty(),
            skip_authority: SkipAuthority::Reached,
        }];
        expected_sources.extend_from_slice(extra_sources);
        Box::pin(run_cycle_with_witness(
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
            None,
        ))
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

    pub(super) async fn finish_mixed_tail(
        &self,
        result: &mut CycleResult,
    ) -> Option<download::OperationalJournalGuards> {
        self.finish_mixed_tail_with(result, |_| {}).await
    }

    pub(super) async fn finish_mixed_tail_with(
        &self,
        result: &mut CycleResult,
        after_prepare: impl FnOnce(bool),
    ) -> Option<download::OperationalJournalGuards> {
        let shutdown = CancellationToken::new();
        result
            .prepare_operational_tail(&self.shared, &shutdown)
            .await
            .unwrap();
        after_prepare(result.operational_tail_prepared());
        let selected = self
            .library_state(
                Session {
                    baseline: Arc::clone(&self.baseline),
                    progress: Arc::clone(&self.progress),
                    hold_child: Arc::clone(&self.hold_child),
                    hold_pair: Arc::clone(&self.hold_pair),
                    followup_child: Arc::clone(&self.followup_child),
                    recovery_events: Arc::clone(&self.recovery_events),
                    zone_name: ZONE.to_owned(),
                    empty: false,
                    cycle: 1,
                    fail_inventory: false,
                    provider_error: false,
                    provider_errors: Arc::new(AtomicUsize::new(0)),
                    changes: Arc::new(AtomicUsize::new(0)),
                    inventory_queries: Arc::new(AtomicUsize::new(0)),
                },
                ZONE,
                &self.key,
            )
            .await;
        crate::sync_loop::runner::drain_source_revalidation(&[&selected], &shutdown)
            .await
            .unwrap();
        result.revalidate_sparse_holds(Some(self.db.as_ref())).await;
        result
            .finish_operational_tail(&self.shared, &shutdown)
            .await
            .unwrap()
    }

    pub(super) async fn drain_recovery_catalog(&self, result: &mut CycleResult) -> usize {
        let selected = self
            .library_state(
                Session {
                    baseline: Arc::clone(&self.baseline),
                    progress: Arc::clone(&self.progress),
                    hold_child: Arc::clone(&self.hold_child),
                    hold_pair: Arc::clone(&self.hold_pair),
                    followup_child: Arc::clone(&self.followup_child),
                    recovery_events: Arc::clone(&self.recovery_events),
                    zone_name: ZONE.to_owned(),
                    empty: false,
                    cycle: 5,
                    fail_inventory: false,
                    provider_error: false,
                    provider_errors: Arc::new(AtomicUsize::new(0)),
                    changes: Arc::new(AtomicUsize::new(0)),
                    inventory_queries: Arc::clone(&self.inventory_queries),
                },
                ZONE,
                &self.key,
            )
            .await;
        let db: Arc<dyn download::DownloadStore> = self.db.clone();
        let base = make_run_cycle_download_config_builder(&self.root, db);
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        let policy = self
            .db
            .get_metadata("enum_config_hash")
            .await
            .unwrap()
            .unwrap();
        let shutdown = CancellationToken::new();
        crate::sync_loop::runner::drain_source_revalidation(&[&selected], &shutdown)
            .await
            .unwrap();
        let drained = crate::sync_loop::runner::drain_catalog_source_revalidation(
            &[&selected],
            &builder,
            download::DownloadControls::download_hidden(),
            &policy,
            &shutdown,
        )
        .await
        .unwrap();
        let retained = drained.retained;
        assert!(!drained.session_expired);
        crate::sync_loop::runner::apply_catalog_drain_outcome(result, &[&selected], drained, false);
        retained
    }

    pub(super) async fn finish_pair_tail(
        &self,
        result: &mut CycleResult,
    ) -> Option<download::OperationalJournalGuards> {
        let shutdown = CancellationToken::new();
        result
            .prepare_operational_tail(&self.shared, &shutdown)
            .await
            .unwrap();
        assert_eq!(self.drain_recovery_catalog(result).await, 0);
        result.revalidate_sparse_holds(Some(self.db.as_ref())).await;
        result
            .finish_operational_tail(&self.shared, &shutdown)
            .await
            .unwrap()
    }

    pub(super) async fn release_account_lock(&self) {
        self.shared.read().await.release_lock().unwrap();
    }

    async fn run_two(&self, cycle: usize, provider_error: bool, peer_first: bool) -> CycleResult {
        self.run_two_filtered(cycle, provider_error, peer_first, false, false)
            .await
    }

    async fn run_two_filtered(
        &self,
        cycle: usize,
        provider_error: bool,
        peer_first: bool,
        skip_peer: bool,
        peer_media: bool,
    ) -> CycleResult {
        self.peer_errors.store(0, Ordering::SeqCst);
        let primary = self
            .library_state(
                Session {
                    baseline: Arc::clone(&self.baseline),
                    progress: Arc::clone(&self.progress),
                    hold_child: Arc::clone(&self.hold_child),
                    hold_pair: Arc::clone(&self.hold_pair),
                    followup_child: Arc::clone(&self.followup_child),
                    recovery_events: Arc::clone(&self.recovery_events),
                    zone_name: ZONE.to_owned(),
                    empty: false,
                    cycle,
                    fail_inventory: false,
                    provider_error: false,
                    provider_errors: Arc::new(AtomicUsize::new(0)),
                    changes: Arc::new(AtomicUsize::new(0)),
                    inventory_queries: Arc::new(AtomicUsize::new(0)),
                },
                ZONE,
                &self.key,
            )
            .await;
        let peer_key = sync_token_key(PEER_ZONE);
        let peer_baseline = if peer_media {
            let mut records = pair(
                "op-peer-baseline",
                "peer-baseline.JPG",
                &format!("{}/peer-baseline", self._server.uri()),
                46,
            );
            records[0]["zoneID"] = zone_for(PEER_ZONE);
            records[1]["zoneID"] = zone_for(PEER_ZONE);
            records[1]["fields"]["masterRef"]["value"]["zoneID"] = zone_for(PEER_ZONE);
            Arc::new(records)
        } else {
            Arc::clone(&self.baseline)
        };
        let mut peer = self
            .library_state(
                Session {
                    baseline: peer_baseline,
                    progress: Arc::clone(&self.progress),
                    hold_child: Arc::clone(&self.hold_child),
                    hold_pair: Arc::clone(&self.hold_pair),
                    followup_child: Arc::clone(&self.followup_child),
                    recovery_events: Arc::clone(&self.recovery_events),
                    zone_name: PEER_ZONE.to_owned(),
                    empty: !peer_media,
                    cycle,
                    fail_inventory: false,
                    provider_error,
                    provider_errors: Arc::clone(&self.peer_errors),
                    changes: Arc::new(AtomicUsize::new(0)),
                    inventory_queries: Arc::new(AtomicUsize::new(0)),
                },
                PEER_ZONE,
                &peer_key,
            )
            .await;
        peer.plan_needs_refresh = skip_peer;
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
        if skip_peer {
            let expected = vec![
                ExpectedSource {
                    zone_name: primary.zone_name.clone(),
                    baseline_scope: primary.library.baseline_scope(),
                    active_passes: true,
                    skip_authority: SkipAuthority::Reached,
                },
                ExpectedSource {
                    zone_name: peer.zone_name.clone(),
                    baseline_scope: peer.library.baseline_scope(),
                    active_passes: true,
                    skip_authority: SkipAuthority::NoChanges,
                },
            ];
            return Box::pin(run_cycle_with_witness(
                &[&primary],
                &self.config,
                Some(self.db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &self.shared,
                &CancellationToken::new(),
                Arc::new(download::MetadataWriterWitness::default()),
                &expected,
                Some(&states),
            ))
            .await
            .unwrap();
        }
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

#[cfg(target_os = "linux")]
#[test]
fn no_hold_actual_disk_failure_is_partial_and_unhealthy() {
    const NAME: &str = "sync_loop::runner::tests::v027_baseline::operational_holds::no_hold_actual_disk_failure_is_partial_and_unhealthy";
    if let Some(root) = std::env::var_os("KEI_TEST_LATE_IO_ROOT") {
        let root = PathBuf::from(root);
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let fixture = Fixture::with_progress_inner(
                |_| ResponseTemplate::new(200).set_body_bytes(MEDIA), true, Some(&root.join("media")), 2,
            ).await.unwrap();
            let baseline = fixture.run(0, false, false).await;
            assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
            let baseline_bytes = std::fs::read(fixture.root.join("baseline.JPG")).unwrap();
            std::fs::write(root.join("fault-enabled"), b"enable").unwrap();
            let mut failed = fixture.run(7, false, false).await;
            let reached = std::fs::read_to_string(root.join("fault-reached")).unwrap();
            assert!(reached.contains("errno=28") && !reached.contains("progress=0"), "{reached}");
            assert_eq!(failed.failed_count, 1, "{failed:?}");
            assert_eq!(failed.stats.failed, 1, "{failed:?}");
            assert!(failed.holds.is_empty(), "{failed:?}");
            assert!(failed.db_sync_token_advance_safe, "{failed:?}");
            assert_eq!(failed.stats.state_write_failures, 0, "{failed:?}");
            assert!(failed.sources.iter().all(|source| source.media_failures.is_empty()));
            {
                let conn = fixture.db.acquire_lock("verify real disk failure").unwrap();
                let (status, error): (String, String) = conn.query_row(
                    "SELECT status,last_error FROM assets WHERE library=?1 AND id='asset-op-progress'",
                    [ZONE], |row| Ok((row.get(0)?,row.get(1)?)),
                ).unwrap();
                assert_eq!(status, "failed");
                assert!(error.contains("space") || error.contains("28"), "{error}");
            }
            assert_eq!(std::fs::read(fixture.root.join("baseline.JPG")).unwrap(), baseline_bytes);
            std::fs::remove_file(root.join("fault-enabled")).unwrap();
            let shutdown = CancellationToken::new();
            failed.prepare_operational_tail(&fixture.shared, &shutdown).await.unwrap();
            let _guards = failed.finish_operational_tail(&fixture.shared, &shutdown).await.unwrap();
            let (health, report) = reported_health(&fixture, &failed, "disk-report.json").await;
            assert_eq!(report["status"], "partial_failure");
            assert_eq!(report["stats"]["failed"], 1);
            assert!(health.last_success_at.is_none(), "real disk failure must not advance health");
        });
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("late-io.so");
    let output = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&shim)
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/late_io_fixture.c"),
        )
        .args(["-ldl", "-pthread"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env("LD_PRELOAD", shim)
        .env("KEI_TEST_LATE_IO_ROOT", dir.path())
        .env("KEI_TEST_LATE_IO_CASE", "write_enospc")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn no_hold_accounted_http_failure_requires_matching_owner_and_journals() {
    let fixture = Fixture::with_progress_inner(|_| ResponseTemplate::new(500), true, None, 2)
        .await
        .unwrap();
    assert!(
        fixture
            .run(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let mut failed = fixture.run(7, false, false).await;
    assert_eq!(
        (failed.failed_count, failed.stats.failed),
        (1, 1),
        "{failed:?}"
    );
    assert!(
        failed.holds.is_empty() && failed.db_sync_token_advance_safe,
        "{failed:?}"
    );
    assert_eq!(failed.sources[0].media_failures.len(), 1);
    let shutdown = CancellationToken::new();
    failed
        .prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    let guards = failed
        .finish_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(
        guards.is_some(),
        "a durably accounted HTTP failure must reach the journal guard"
    );
    let (health, report) = reported_health(&fixture, &failed, "http-accounted.json").await;
    assert_eq!(report["status"], "partial_failure");
    assert!(health.last_success_at.is_some());
    drop(guards);
    failed.sources[0].media_failures[0]
        .successor
        .committed_admission_hash
        .push('0');
    assert!(
        failed
            .finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    let (health, report) = reported_health(&fixture, &failed, "http-stale.json").await;
    assert_eq!(report["status"], "partial_failure");
    assert!(
        health.last_success_at.is_none(),
        "stale accounting cannot retain preliminary health authority"
    );
}

pub(super) async fn reported_health(
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
    let primary_before = fixture.db.get_metadata(&fixture.key).await.unwrap();
    let peer_key = sync_token_key(PEER_ZONE);
    let peer_before = fixture.db.get_metadata(&peer_key).await.unwrap();
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
                && hold.ordinal == 0
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
    assert_eq!(fixture.peer_errors.load(Ordering::SeqCst), 2);
    assert!(held.holds.iter().any(|hold| {
        hold.source == PEER_ZONE
            && hold.ordinal == 1
            && hold.kind == crate::sync_cycle::LibraryHoldKind::RetainedCheckpoint
            && hold.reason == "retained_checkpoint_expired"
    }));
    assert!(!held.holds.iter().any(|hold| {
        hold.source == ZONE && hold.kind == crate::sync_cycle::LibraryHoldKind::RetainedCheckpoint
    }));
    assert_eq!(
        fixture.db.get_metadata(&fixture.key).await.unwrap(),
        primary_before
    );
    assert_eq!(
        fixture.db.get_metadata(&peer_key).await.unwrap(),
        peer_before
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
    let primary_before = fixture.db.get_metadata(&fixture.key).await.unwrap();
    let peer_key = sync_token_key(PEER_ZONE);
    let peer_before = fixture.db.get_metadata(&peer_key).await.unwrap();
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
                && hold.ordinal == 1
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
    assert_eq!(fixture.peer_errors.load(Ordering::SeqCst), 2);
    assert!(held.holds.iter().any(|hold| {
        hold.source == PEER_ZONE
            && hold.ordinal == 0
            && hold.kind == crate::sync_cycle::LibraryHoldKind::RetainedCheckpoint
            && hold.reason == "retained_checkpoint_expired"
    }));
    assert!(!held.holds.iter().any(|hold| {
        hold.source == ZONE && hold.kind == crate::sync_cycle::LibraryHoldKind::RetainedCheckpoint
    }));
    assert_eq!(
        held.stats.sync_token_blocked_zone.as_deref(),
        Some(PEER_ZONE)
    );
    assert_eq!(
        held.stats.sync_token_blocked_reason,
        Some("retained_checkpoint_expired")
    );
    assert_eq!(
        fixture.db.get_metadata(&fixture.key).await.unwrap(),
        primary_before
    );
    assert_eq!(
        fixture.db.get_metadata(&peer_key).await.unwrap(),
        peer_before
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

#[tokio::test]
async fn quiet_cycle_resets_per_sync_asset_counter_but_retains_future_decision() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic provider fixture");
    assert!(
        fixture
            .run(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let mixed = fixture.run(1, false, false).await;
    assert_eq!(mixed.stats.failed, 1, "{mixed:?}");
    let checkpoint = fixture.db.get_metadata(&fixture.key).await.unwrap();
    let read_state = |db: &SqliteStateDb| {
        let conn = db.acquire_lock("read quiet retry projection").unwrap();
        let asset: (String, i64, Option<String>) = conn.query_row(
            "SELECT status,download_attempts,last_error FROM assets WHERE library=?1 AND id='asset-op-progress'",
            [ZONE], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        let decision: (i64, i64, String) = conn.query_row(
            "SELECT d.attempts,d.next_retry_at,json_array(d.generation,d.pass_key,d.child,d.manifest_hash,d.admission,d.admission_hash,w.path,w.checksum,w.size_bytes,w.admitted,w.verified_media) FROM provider_active_decisions d JOIN provider_active_destinations w USING(generation,pass_key,child) WHERE w.library=?1 AND w.asset_id='asset-op-progress' AND w.verified_media=0",
            [ZONE], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        (asset, decision)
    };
    let before = read_state(&fixture.db);
    assert_eq!(before.0.0, "failed");
    assert!(before.0.1 > 0 && before.0.2.is_some());
    assert!(before.1.0 > 0 && before.1.1 > chrono::Utc::now().timestamp());
    let requests_before = fixture
        ._server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/progress")
        .count();
    let inventory_before = fixture.inventory_queries.load(Ordering::SeqCst);
    assert!(requests_before > 0);
    drop(mixed);
    let fixture = fixture.reopen().await;
    // The production last-seen marker has one-second resolution. A later
    // watch wake must not share the preceding cycle's timestamp.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let quiet = fixture.run(2, false, false).await;
    assert_eq!(quiet.stats.failed, 0, "{quiet:?}");
    let after = read_state(&fixture.db);
    assert_eq!(after.0, ("pending".to_owned(), 0, None));
    assert_eq!(after.1, before.1);
    assert_eq!(
        fixture.db.get_metadata(&fixture.key).await.unwrap(),
        checkpoint
    );
    let requests_after = fixture
        ._server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/progress")
        .count();
    assert_eq!(requests_after, requests_before);
    assert_eq!(
        fixture.inventory_queries.load(Ordering::SeqCst),
        inventory_before
    );
    assert!(!fixture.root.join("progress.JPG").exists());
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn skipped_peer_foreign_linked_catalog_debt_refuses_capture() {
    for missing in [false, true] {
        let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
            .await
            .expect("synthetic peer fixture");
        Mock::given(method("GET"))
            .and(path("/peer-baseline"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&fixture._server)
            .await;
        let baseline = fixture.run_two_filtered(0, false, false, false, true).await;
        assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
        let expiry = fixture.run_two_filtered(3, false, false, false, true).await;
        assert!(expiry.can_advance_database_checkpoint(), "{expiry:?}");
        let descriptor_key = format!("observed_selection:v27:{PEER_ZONE}");
        let descriptor = fixture
            .db
            .get_metadata(&descriptor_key)
            .await
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&descriptor).unwrap();
        let round = value["revalidation_round"]
            .as_str()
            .expect("owner-linked round")
            .to_owned();
        let peer_key = sync_token_key(PEER_ZONE);
        let token = fixture.db.get_metadata(&peer_key).await.unwrap();
        let debt = {
            let conn = fixture
                .db
                .acquire_lock("foreign linked catalog fault")
                .unwrap();
            let debt: (String, i64) = conn
                .query_row(
                    "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 LIMIT 1",
                    [&round],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("real owner-created linked debt");
            if missing {
                conn.pragma_update(None, "foreign_keys", false).unwrap();
                assert_eq!(
                    conn.execute(
                        "UPDATE source_revalidation_rounds SET id=?2 WHERE id=?1 AND library=?3",
                        rusqlite::params![round, format!("hidden-{round}"), PEER_ZONE]
                    )
                    .unwrap(),
                    1
                );
                conn.pragma_update(None, "foreign_keys", true).unwrap();
            } else {
                assert_eq!(conn.execute("UPDATE source_revalidation_rounds SET library='ForeignSyntheticZone' WHERE id=?1 AND library=?2", rusqlite::params![round, PEER_ZONE]).unwrap(), 1);
            }
            debt
        };
        let mut held = fixture.run_two_filtered(4, false, false, true, true).await;
        let peer = held
            .sources
            .iter()
            .find(|source| source.zone_name == PEER_ZONE)
            .unwrap();
        assert!(!peer.reached && !peer.ordinary_committed);
        assert!(
            peer.operational_prework.is_none(),
            "foreign linked round must refuse capture"
        );
        let shutdown = CancellationToken::new();
        held.prepare_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap();
        assert!(
            held.finish_operational_tail(&fixture.shared, &shutdown)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(fixture.db.get_metadata(&peer_key).await.unwrap(), token);
        assert_eq!(
            fixture
                .db
                .get_metadata(&descriptor_key)
                .await
                .unwrap()
                .as_deref(),
            Some(descriptor.as_str())
        );
        let after: (String, i64) = fixture
            .db
            .acquire_lock("check retained linked debt")
            .unwrap()
            .query_row(
                "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 LIMIT 1",
                [&round],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, debt);
        if missing {
            let conn = fixture
                .db
                .acquire_lock("restore hidden linked catalog row")
                .unwrap();
            conn.pragma_update(None, "foreign_keys", false).unwrap();
            assert_eq!(
                conn.execute(
                    "UPDATE source_revalidation_rounds SET id=?1 WHERE id=?2",
                    rusqlite::params![round, format!("hidden-{round}")]
                )
                .unwrap(),
                1
            );
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            let intact: i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.id=?1", [&round], |row| row.get(0)).unwrap();
            assert!(intact > 0);
        }
        assert_eq!(
            std::fs::read(fixture.root.join("peer-baseline.JPG")).unwrap(),
            MEDIA
        );
    }
}

#[tokio::test]
async fn skipped_peer_linked_empty_round_drift_after_prepare_vetoes_health() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    assert!(
        fixture
            .run_two(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let descriptor_key = format!("observed_selection:v27:{PEER_ZONE}");
    let descriptor = fixture
        .db
        .get_metadata(&descriptor_key)
        .await
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_str(&descriptor).unwrap();
    let round = value["revalidation_round"]
        .as_str()
        .expect("owner-linked round")
        .to_owned();
    let token_key = sync_token_key(PEER_ZONE);
    let token = fixture.db.get_metadata(&token_key).await.unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(held.operational_tail_prepared());
    {
        let conn = fixture.db.acquire_lock("drift linked empty round").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1",
                [&round],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "this test covers a linked empty census only");
        assert_eq!(conn.execute("UPDATE source_revalidation_rounds SET library='ForeignSyntheticZone' WHERE id=?1 AND library=?2", rusqlite::params![round, PEER_ZONE]).unwrap(), 1);
    }
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    let (health, report) = reported_health(&fixture, &held, "linked-round-drift.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 1);
    assert_eq!(fixture.db.get_metadata(&token_key).await.unwrap(), token);
    assert_eq!(
        fixture
            .db
            .get_metadata(&descriptor_key)
            .await
            .unwrap()
            .as_deref(),
        Some(descriptor.as_str())
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn skipped_peer_current_empty_link_allows_unrelated_foreign_history() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    assert!(
        fixture
            .run_two(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let descriptor_key = format!("observed_selection:v27:{PEER_ZONE}");
    let descriptor = fixture
        .db
        .get_metadata(&descriptor_key)
        .await
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_str(&descriptor).unwrap();
    assert!(
        value["revalidation_round"].as_str().is_some(),
        "baseline owns an ACTIVE catalog link"
    );
    let scope = fixture
        .db
        .get_metadata(&format!("download_ledger_scope:{PEER_ZONE}"))
        .await
        .unwrap()
        .unwrap()
        .replace(PEER_ZONE, "ForeignSyntheticZone");
    let policy = fixture
        .db
        .get_metadata("enum_config_hash")
        .await
        .unwrap()
        .unwrap();
    fixture
        .db
        .seed_source_revalidation(
            baseline_owner(),
            "ForeignSyntheticZone".to_owned(),
            scope,
            policy,
            "foreign-boundary".to_owned(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let foreign: i64 = fixture
        .db
        .acquire_lock("check foreign history")
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library='ForeignSyntheticZone'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(foreign, 1);
    let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
    assert_eq!(held.stats.failed, 1);
    // The aggregate cycle has already reported its own facts. Add an
    // unrelated hold afterward to isolate the skipped-source owner guard.
    crate::sync_cycle::persist_test_retained_checkpoint_hold(
        fixture.db.as_ref(),
        "ForeignSyntheticZone",
        "foreign-cursor",
        &download::compute_config_hash(&fixture.config),
        &"b".repeat(64),
    )
    .await
    .unwrap();
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(
        guards.is_some(),
        "unrelated foreign history must not veto the current empty link"
    );
    let (health, report) = reported_health(&fixture, &held, "foreign-history-health.json").await;
    assert!(health.last_success_at.is_some());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 1);
    assert_eq!(
        fixture
            .db
            .get_metadata(&descriptor_key)
            .await
            .unwrap()
            .as_deref(),
        Some(descriptor.as_str())
    );
}

fn fixture_hold_preservation_hash(fixture: &Fixture, zones: &[String]) -> String {
    let db: Arc<dyn download::DownloadStore> = fixture.db.clone();
    let builder = make_run_cycle_download_config_builder(&fixture.root, db);
    let mut probe = (*builder(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from(ZONE),
    ))
    .clone();
    probe.folder_structure = "none".into();
    probe.retry.max_retries = 0;
    let enum_hash = download::compute_config_hash(&fixture.config);
    let download_hash = download::hash_download_config(&probe);
    let coverage = download::sync_coverage_fingerprint_json(
        &fixture.config,
        "icloud",
        1,
        zones,
        &enum_hash,
        &download_hash,
    )
    .unwrap();
    let policy = json!([
        1,
        crate::state::METADATA_CAPTURE_REVISION,
        coverage,
        format!("{:?}", probe.metadata),
        fixture.config.runtime.refresh_metadata,
        fixture.config.runtime.repair_capture_timestamps,
        fixture.config.runtime.repair_truncated
    ]);
    crate::sync_cycle::hash_legacy_preservation_policy(
        &policy,
        probe.legacy_preservation_allow_hardlinks,
    )
}

#[tokio::test]
async fn skipped_peer_unrelated_real_hold_does_not_veto_health() {
    for different_cursor in [true, false] {
        let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
            .await
            .expect("synthetic peer fixture");
        assert!(
            fixture
                .run_two(0, false, false)
                .await
                .can_advance_database_checkpoint()
        );
        let token_key = sync_token_key(PEER_ZONE);
        let cursor = fixture.db.get_metadata(&token_key).await.unwrap().unwrap();
        let enum_hash = download::compute_config_hash(&fixture.config);
        assert_eq!(enum_hash.len(), 16, "actual enumeration hash shape");
        let mut other_config = make_run_cycle_config();
        other_config.photos.edited = !other_config.photos.edited;
        let other_enum_hash = download::compute_config_hash(&other_config);
        assert_eq!(other_enum_hash.len(), 16);
        assert_ne!(other_enum_hash, enum_hash);
        let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
        let shutdown = CancellationToken::new();
        held.prepare_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap();
        assert!(held.operational_tail_prepared());
        let preservation =
            fixture_hold_preservation_hash(&fixture, &[PEER_ZONE.to_owned(), ZONE.to_owned()]);
        crate::sync_cycle::persist_test_retained_checkpoint_hold(
            fixture.db.as_ref(),
            PEER_ZONE,
            if different_cursor {
                "other-real-cursor"
            } else {
                &cursor
            },
            if different_cursor {
                &enum_hash
            } else {
                &other_enum_hash
            },
            &preservation,
        )
        .await
        .unwrap();
        let hold_key = format!(
            "{}{PEER_ZONE}",
            crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX
        );
        let hold = fixture.db.get_metadata(&hold_key).await.unwrap().unwrap();
        let guards = held
            .finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap();
        assert!(
            guards.is_some(),
            "valid same-zone hold for another cursor or enum policy is unrelated"
        );
        let (health, report) = reported_health(&fixture, &held, "unrelated-peer-hold.json").await;
        assert!(health.last_success_at.is_some());
        assert_eq!(report["status"], "partial_failure");
        assert_eq!(report["stats"]["failed"], 1);
        assert_eq!(
            fixture.db.get_metadata(&hold_key).await.unwrap().as_deref(),
            Some(hold.as_str())
        );
        assert_eq!(
            fixture
                .db
                .get_metadata(&token_key)
                .await
                .unwrap()
                .as_deref(),
            Some(cursor.as_str())
        );
        assert_eq!(
            std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
            MEDIA
        );
    }
}

#[tokio::test]
async fn skipped_peer_real_rejected_cursor_hold_from_reached_wake_vetoes_health() {
    use sha2::{Digest, Sha256};

    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    assert!(
        fixture
            .run_two(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let token_key = sync_token_key(PEER_ZONE);
    let cursor = fixture.db.get_metadata(&token_key).await.unwrap().unwrap();
    let descriptor_key = format!("observed_selection:v27:{PEER_ZONE}");
    let descriptor = fixture.db.get_metadata(&descriptor_key).await.unwrap();
    let epoch_key = crate::state::db::sync_epoch::library_key(PEER_ZONE);
    let epoch = fixture.db.get_metadata(&epoch_key).await.unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
    assert_eq!(held.stats.failed, 1);
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(held.operational_tail_prepared());
    let prework = held
        .sources
        .iter()
        .find(|source| source.zone_name == PEER_ZONE)
        .unwrap()
        .operational_prework
        .as_ref()
        .unwrap()
        .prework
        .clone();
    assert!(
        fixture
            .db
            .operational_accounting(prework.clone(), vec![], None, false)
            .await
            .unwrap()
            .is_some()
    );
    let rejected = fixture.run_two(1, true, false).await;
    assert!(
        fixture.peer_errors.load(Ordering::SeqCst) > 0,
        "actual peer rejection"
    );
    assert!(rejected.holds.iter().any(|hold| hold.source == PEER_ZONE
        && hold.kind == crate::sync_cycle::LibraryHoldKind::RetainedCheckpoint));
    let hold_key = format!(
        "{}{PEER_ZONE}",
        crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX
    );
    let raw = fixture
        .db
        .get_metadata(&hold_key)
        .await
        .unwrap()
        .expect("owner-persisted peer hold");
    let hold: Value = serde_json::from_str(&raw).unwrap();
    let enum_hash = download::compute_config_hash(&fixture.config);
    assert_eq!(enum_hash.len(), 16);
    assert_eq!(hold["enum_config_hash"], enum_hash);
    assert_eq!(
        hold["prior_cursor_sha256"],
        data_encoding::HEXLOWER.encode(&Sha256::digest(cursor.as_bytes()))
    );
    assert_eq!(
        hold["preservation_config_hash"],
        fixture_hold_preservation_hash(&fixture, &[PEER_ZONE.to_owned(), ZONE.to_owned()])
    );
    assert_ne!(
        hold["preservation_config_hash"],
        fixture_hold_preservation_hash(&fixture, &[ZONE.to_owned()])
    );
    assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
    assert_eq!(
        fixture
            .db
            .get_metadata(&token_key)
            .await
            .unwrap()
            .as_deref(),
        Some(cursor.as_str())
    );
    assert_eq!(
        fixture.db.get_metadata(&descriptor_key).await.unwrap(),
        descriptor
    );
    assert_eq!(fixture.db.get_metadata(&epoch_key).await.unwrap(), epoch);
    drop(rejected);
    assert!(
        crate::sync_cycle::retained_checkpoint_blocks_health(&raw, &cursor, &enum_hash),
        "the actual reached-peer hold must not be called unrelated merely because the visit set changed"
    );
    assert!(
        fixture
            .db
            .operational_accounting(prework, vec![], None, false)
            .await
            .unwrap()
            .is_none(),
        "real reached-peer hold must veto the skipped peer owner despite changed visit set"
    );
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    let (health, report) = reported_health(&fixture, &held, "real-reached-peer-hold.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 1);
    assert_eq!(
        fixture.db.get_metadata(&hold_key).await.unwrap().as_deref(),
        Some(raw.as_str())
    );
    assert_eq!(
        fixture
            .db
            .get_metadata(&token_key)
            .await
            .unwrap()
            .as_deref(),
        Some(cursor.as_str())
    );
    assert_eq!(
        fixture.db.get_metadata(&descriptor_key).await.unwrap(),
        descriptor
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn skipped_peer_matching_retained_hold_after_prepare_vetoes_health() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    assert!(
        fixture
            .run_two(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let key = sync_token_key(PEER_ZONE);
    let cursor = fixture.db.get_metadata(&key).await.unwrap().unwrap();
    let descriptor_key = format!("observed_selection:v27:{PEER_ZONE}");
    let descriptor = fixture.db.get_metadata(&descriptor_key).await.unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
    assert_eq!(held.stats.failed, 1);
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(held.operational_tail_prepared());
    let enum_hash = download::compute_config_hash(&fixture.config);
    assert_eq!(enum_hash.len(), 16, "actual enumeration hash shape");
    let preservation_hash =
        fixture_hold_preservation_hash(&fixture, &[PEER_ZONE.to_owned(), ZONE.to_owned()]);
    crate::sync_cycle::persist_test_retained_checkpoint_hold(
        fixture.db.as_ref(),
        PEER_ZONE,
        &cursor,
        &enum_hash,
        &preservation_hash,
    )
    .await
    .unwrap();
    let hold_key = format!(
        "{}{PEER_ZONE}",
        crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX
    );
    let hold = fixture.db.get_metadata(&hold_key).await.unwrap().unwrap();
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!held.operational_safe_holds);
    let (health, report) =
        reported_health(&fixture, &held, "skipped-peer-retained-hold.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 1);
    assert_eq!(
        fixture.db.get_metadata(&hold_key).await.unwrap().as_deref(),
        Some(hold.as_str())
    );
    assert_eq!(
        fixture.db.get_metadata(&key).await.unwrap().as_deref(),
        Some(cursor.as_str())
    );
    assert_eq!(
        fixture.db.get_metadata(&descriptor_key).await.unwrap(),
        descriptor
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn skipped_peer_active_observation_is_required_and_rechecked() {
    for invalidate_after_prepare in [false, true] {
        let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
            .await
            .expect("synthetic peer fixture");
        let baseline = fixture.run_two(0, false, false).await;
        assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
        let peer_key = sync_token_key(PEER_ZONE);
        let checkpoint = fixture.db.get_metadata(&peer_key).await.unwrap();
        let descriptor_key = format!("observed_selection:v27:{PEER_ZONE}");
        assert!(
            fixture
                .db
                .get_metadata(&descriptor_key)
                .await
                .unwrap()
                .is_some()
        );
        if !invalidate_after_prepare {
            fixture
                .db
                .set_metadata(&descriptor_key, "invalid")
                .await
                .unwrap();
        }
        let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
        assert_eq!(held.stats.failed, 1, "{held:?}");
        let peer = held
            .sources
            .iter()
            .find(|source| source.zone_name == PEER_ZONE)
            .unwrap();
        assert!(!peer.reached && !peer.ordinary_committed);
        assert_eq!(peer.operational_prework.is_some(), invalidate_after_prepare);
        let shutdown = CancellationToken::new();
        held.prepare_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap();
        assert_eq!(held.operational_tail_prepared(), invalidate_after_prepare);
        if invalidate_after_prepare {
            fixture
                .db
                .set_metadata(&descriptor_key, "invalid")
                .await
                .unwrap();
        }
        assert!(
            held.finish_operational_tail(&fixture.shared, &shutdown)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!held.operational_safe_holds);
        let (health, report) =
            reported_health(&fixture, &held, "skipped-peer-invalid-active.json").await;
        assert!(health.last_success_at.is_none());
        assert_eq!(report["status"], "partial_failure");
        assert_eq!(report["stats"]["failed"], 1);
        assert_eq!(
            fixture.db.get_metadata(&peer_key).await.unwrap(),
            checkpoint
        );
        assert_eq!(
            std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
            MEDIA
        );
    }
}

#[tokio::test]
async fn skipped_peer_new_unknown_sparse_debt_vetoes_guarded_health() {
    use crate::state::{SparseEvidence, SparseSourceId};

    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    let baseline = fixture.run_two(0, false, false).await;
    assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
    let peer_key = sync_token_key(PEER_ZONE);
    let checkpoint = fixture.db.get_metadata(&peer_key).await.unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(held.operational_tail_prepared());
    let source = SparseSourceId::new("synthetic-unknown-peer-child");
    fixture
        .db
        .observe_sparse_identity(
            PEER_ZONE,
            &source,
            &SparseEvidence::new("synthetic-unknown".to_owned()),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!held.operational_safe_holds);
    assert_eq!(
        fixture.db.sparse_identities(PEER_ZONE).await.unwrap().len(),
        1
    );
    assert_eq!(
        fixture.db.get_metadata(&peer_key).await.unwrap(),
        checkpoint
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn skipped_peer_rechecks_the_exact_valid_active_observation() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    assert!(
        fixture
            .run_two(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let peer_key = sync_token_key(PEER_ZONE);
    let checkpoint = fixture.db.get_metadata(&peer_key).await.unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(held.operational_tail_prepared());
    let key = format!("observed_selection:v27:{PEER_ZONE}");
    let original = fixture.db.get_metadata(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_str(&original).unwrap();
    let changed = serde_json::to_string_pretty(&value).unwrap();
    assert_ne!(original, changed);
    {
        let conn = fixture
            .db
            .acquire_lock("change peer descriptor bytes")
            .unwrap();
        conn.execute(
            "UPDATE metadata SET value=?1 WHERE key=?2",
            rusqlite::params![changed, key],
        )
        .unwrap();
        let peer = held
            .sources
            .iter()
            .find(|source| source.zone_name == PEER_ZONE)
            .unwrap();
        let evidence = peer
            .operational_prework
            .as_ref()
            .unwrap()
            .prework
            .census_evidence_for_test(&conn)
            .unwrap();
        assert!(
            evidence.1,
            "the logically unchanged descriptor remains valid for its real source"
        );
    }
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!held.operational_safe_holds);
    assert_eq!(
        fixture.db.get_metadata(&peer_key).await.unwrap(),
        checkpoint
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn skipped_peer_owner_pin_drift_refuses_mixed_health() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    assert!(
        fixture
            .run_two(0, false, false)
            .await
            .can_advance_database_checkpoint()
    );
    let checkpoint = fixture
        .db
        .get_metadata(&sync_token_key(PEER_ZONE))
        .await
        .unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, false).await;
    let peer = held
        .sources
        .iter_mut()
        .find(|source| source.zone_name == PEER_ZONE)
        .unwrap();
    let prework = peer
        .operational_prework
        .as_mut()
        .expect("actual skipped owner prework");
    let other =
        serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"other-provider"}})).unwrap();
    prework.owner = crate::state::db::account::AccountOwner::authenticated(
        "baseline@example.invalid",
        "com",
        &other,
    )
    .unwrap();
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(!held.operational_tail_prepared());
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!held.operational_safe_holds);
    assert_eq!(
        fixture
            .db
            .get_metadata(&sync_token_key(PEER_ZONE))
            .await
            .unwrap(),
        checkpoint
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn skipped_peer_unclassified_owner_debt_is_retained() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    Mock::given(method("GET"))
        .and(path("/peer-baseline"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&fixture._server)
        .await;
    assert!(
        fixture
            .run_two_filtered(0, false, false, false, true)
            .await
            .can_advance_database_checkpoint()
    );
    let checkpoint = fixture
        .db
        .get_metadata(&sync_token_key(PEER_ZONE))
        .await
        .unwrap()
        .unwrap();
    let scope = fixture
        .db
        .get_metadata(&format!("download_ledger_scope:{PEER_ZONE}"))
        .await
        .unwrap()
        .unwrap();
    let policy = fixture
        .db
        .get_metadata("enum_config_hash")
        .await
        .unwrap()
        .unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, true).await;
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(held.operational_tail_prepared());
    fixture
        .db
        .seed_source_revalidation(
            baseline_owner(),
            PEER_ZONE.to_owned(),
            scope,
            policy,
            checkpoint.clone(),
            shutdown.clone(),
        )
        .await
        .unwrap();
    let before = {
        let conn = fixture
            .db
            .acquire_lock("simulate unknown peer census mode")
            .unwrap();
        let changed = conn.execute(
            "UPDATE source_revalidation_rounds SET mode='future_catalog_mode' WHERE library=?1 AND phase='observing'",
            [PEER_ZONE],
        ).unwrap();
        assert!(changed > 0);
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.mode='future_catalog_mode'",
            [PEER_ZONE], |row| row.get(0),
        ).unwrap();
        assert!(count > 0, "owner-created peer debt must exist");
        count
    };
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!held.operational_safe_holds);
    let after: i64 = fixture.db.acquire_lock("verify unknown peer debt").unwrap().query_row(
        "SELECT count(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1 AND r.mode='future_catalog_mode'",
        [PEER_ZONE], |row| row.get(0),
    ).unwrap();
    assert_eq!(after, before);
    assert_eq!(
        fixture
            .db
            .get_metadata(&sync_token_key(PEER_ZONE))
            .await
            .unwrap()
            .as_deref(),
        Some(checkpoint.as_str())
    );
    assert_eq!(
        std::fs::read(fixture.root.join("peer-baseline.JPG")).unwrap(),
        MEDIA
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn skipped_peer_real_journal_vetoes_mixed_health() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("synthetic peer fixture");
    Mock::given(method("GET"))
        .and(path("/peer-baseline"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&fixture._server)
        .await;
    let baseline = fixture.run_two_filtered(0, false, false, false, true).await;
    assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
    let target = fixture.root.join("peer-baseline.JPG");
    assert_eq!(std::fs::read(&target).unwrap(), MEDIA);
    let checkpoint = fixture
        .db
        .get_metadata(&sync_token_key(PEER_ZONE))
        .await
        .unwrap();
    let mut held = fixture.run_two_filtered(1, false, false, true, true).await;
    let shutdown = CancellationToken::new();
    held.prepare_operational_tail(&fixture.shared, &shutdown)
        .await
        .unwrap();
    assert!(held.operational_tail_prepared());
    let part = fixture.root.join("peer-journal.part");
    crate::download::file::seed_test_replacement_journal(
        &target,
        &part,
        MEDIA,
        b"synthetic replacement",
    )
    .unwrap();
    assert!(
        held.finish_operational_tail(&fixture.shared, &shutdown)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!held.operational_safe_holds);
    assert_eq!(std::fs::read(&target).unwrap(), MEDIA);
    assert_eq!(std::fs::read(&part).unwrap(), b"synthetic replacement");
    assert_eq!(
        fixture
            .db
            .get_metadata(&sync_token_key(PEER_ZONE))
            .await
            .unwrap(),
        checkpoint
    );
}
