//! Current selected files do not depend on cross-library attribution receipts.
use super::{MEDIA, baseline_owner, pair};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::{SparseIdentityStore as _, SqliteStateDb};
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
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

const PRIMARY: &str = "PrimarySync-current-coverage";
const SHARED: &str = "SharedSync-current-coverage";
fn zone(shared: bool) -> Value {
    json!({"zoneName":if shared {SHARED} else {PRIMARY},
        "ownerRecordName":if shared {"shared-owner"} else {"_defaultOwner"}})
}
fn sparse(mode: u8) -> Value {
    let mut record = crate::test_helpers::sparse_shared_asset_record();
    record["recordName"] = json!("asset-primary");
    record["fields"]["linkedShareRecordName"]["value"] = json!(if mode == 1 || mode == 3 {
        "asset-hidden-0"
    } else {
        "asset-shared-0"
    });
    record["fields"]["linkedShareZoneName"]["value"] = json!(SHARED);
    record["fields"]["linkedShareZoneOwner"]["value"]["recordName"] = json!(if mode == 2 {
        "foreign-owner"
    } else {
        "shared-owner"
    });
    record
}
#[derive(Clone)]
struct Session {
    mode: u8,
    cycle: usize,
    shared: bool,
    records: Arc<Vec<Value>>,
    queries: Arc<AtomicUsize>,
    lookups: Arc<AtomicUsize>,
}
impl Session {
    fn selected(&self, hidden: bool) -> Vec<Value> {
        self.records
            .chunks(2)
            .filter(|pair| {
                let is_hidden = pair[1]["fields"]["isHidden"]["value"].as_i64() == Some(1);
                is_hidden == hidden
            })
            .flat_map(|pair| pair.iter().cloned())
            .collect()
    }
}
#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(self.shared),"syncToken":"boundary","deleted":false}]}),
            );
        }
        let hidden = body.contains("HiddenByAssetDate");
        if url.contains("/internal/records/query/batch") {
            let batch: Vec<_> = request["batch"].as_array().unwrap().iter().map(|query| {
                let hidden = query.to_string().contains("HiddenByAssetDate");
                json!({"records":[{"fields":{"itemCount":{"value":self.selected(hidden).len()/2}}}]})
            }).collect();
            return Ok(json!({"batch":batch}));
        }
        if url.contains("/records/query?") {
            if request["query"]["recordType"]
                .as_str()
                .is_some_and(|kind| kind.contains("Container"))
            {
                return Ok(json!({"records":[],"syncToken":"containers"}));
            }
            assert_eq!(request["zoneID"], zone(self.shared));
            self.queries.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset == 0 {self.selected(hidden)} else {vec![]},"syncToken":"query-only"}),
            );
        }
        if url.contains("/records/lookup?") {
            assert_eq!(request["zoneID"], zone(self.shared));
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let requested = request["records"].as_array().unwrap();
            let records = if !self.shared && self.cycle > 0 {
                vec![sparse(self.mode)]
            } else {
                self.records.as_ref().clone()
            };
            let records: Vec<_> = records
                .into_iter()
                .filter(|r| requested.iter().any(|q| q["recordName"] == r["recordName"]))
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone(self.shared));
            let records = if self.cycle == 1 {
                if self.shared {
                    self.records[4..].to_vec()
                } else {
                    vec![sparse(self.mode)]
                }
            } else {
                vec![]
            };
            return Ok(
                json!({"zones":[{"zoneID":zone(self.shared),"records":records,
                "syncToken":format!("current-after-{}",self.cycle),"moreComing":false}]}),
            );
        }
        anyhow::bail!("unexpected current coverage endpoint")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn current_primary_sparse_shared_hidden_coverage_without_attribution() {
    coverage_case(0).await;
}
async fn coverage_case(mode: u8) {
    let server = crate::start_wiremock_or_skip!();
    let provider_cycle = Arc::new(AtomicUsize::new(0));
    let pending_successes = Arc::new(AtomicUsize::new(0));
    if mode == 4 {
        let cycle = provider_cycle.clone();
        let successes = pending_successes.clone();
        Mock::given(method("GET"))
            .and(path("/shared-0"))
            .respond_with(move |_: &wiremock::Request| {
                if cycle.load(Ordering::SeqCst) < 2 {
                    ResponseTemplate::new(503)
                } else {
                    successes.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_bytes(MEDIA)
                }
            })
            .mount(&server)
            .await;
    }
    for name in ["primary", "shared-0", "hidden-0", "shared-1", "hidden-1"] {
        if (mode == 4 && name == "shared-0") || (mode == 3 && name.starts_with("hidden")) {
            continue;
        }
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
    let (_, shared_session) = make_shared_session_for_run_cycle().await;
    let mut timestamps = Vec::new();
    let mut source_cursors = Vec::new();
    for cycle in 0..5 {
        provider_cycle.store(cycle, Ordering::SeqCst);
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if mode == 4 && cycle >= 2 {
            db.make_selection_retry_due(baseline_owner(), "asset-shared-0".into())
                .await
                .unwrap();
            for row in db.sparse_identities(PRIMARY).await.unwrap() {
                db.record_sparse_attempt(
                    &row,
                    crate::state::SparseAttemptOutcome::Unresolved(row.observed.clone()),
                    chrono::Utc::now() - chrono::TimeDelta::days(100),
                )
                .await
                .unwrap();
            }
        }
        let mut states = Vec::new();
        let mut probes = Vec::new();
        for shared in [false, true] {
            let mut records = Vec::new();
            let names = if shared {
                vec!["shared-0", "hidden-0", "shared-1", "hidden-1"]
            } else {
                vec!["primary"]
            };
            for (index, name) in names.iter().enumerate() {
                if cycle == 0 && index >= 2 {
                    continue;
                }
                let mut entry = pair(
                    name,
                    &format!("{name}.JPG"),
                    &format!("{}/{name}", server.uri()),
                    90 + u8::from(shared) * 10 + index as u8,
                );
                entry[1]["fields"]["masterRef"]["value"]["zoneID"] = zone(shared);
                if name.starts_with("hidden") {
                    entry[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
                }
                if mode == 5 && cycle > 0 && *name == "shared-0" {
                    entry[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/missing-companion",server.uri()),"size":MEDIA.len(),"fileChecksum":entry[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"]}});
                    entry[0]["fields"]["resOriginalVidComplFileType"] =
                        json!({"value":"com.apple.quicktime-movie"});
                }
                records.extend(entry);
            }
            let session = Session {
                mode,
                cycle,
                shared,
                records: Arc::new(records),
                queries: Arc::default(),
                lookups: Arc::default(),
            };
            probes.push(session.clone());
            let mut library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(session),
                Arc::new(zone(shared)),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
            let mut passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::default(),
            }];
            if shared && mode != 3 {
                passes.push(AlbumPass {
                    kind: PassKind::SmartFolder,
                    album: library.albums().await.unwrap().remove("Hidden").unwrap(),
                    exclude_ids: Arc::default(),
                });
            }
            let name = if shared { SHARED } else { PRIMARY };
            let mut state =
                make_run_cycle_library_state_with_passes(name, &sync_token_key(name), passes);
            state.library = library;
            states.push(state);
        }
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, library| {
            let mut config = (*base(mode, exclude, groups, library)).clone();
            config.folder_structure = "none".into();
            config.retry.max_retries = 0;
            config.folder_structure_smart_folders = "hidden".into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = false;
            }
            Arc::new(config)
        };
        let result = run_cycle(
            &[&states[0], &states[1]],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 {
                3 - usize::from(mode == 3 || mode == 4)
            } else if cycle == 1 {
                2 - usize::from(mode == 3)
            } else {
                usize::from(mode == 4 && cycle == 2)
            },
            "{result:?}"
        );
        if mode == 0 && cycle == 0 {
            db.test_current_coverage_checkpoint_fences("asset-shared-0")
                .await;
        }
        for name in ["primary", "shared-0", "hidden-0", "shared-1", "hidden-1"] {
            if cycle == 0 && name.ends_with('1') {
                continue;
            }
            if (mode == 3 && name.starts_with("hidden"))
                || (mode == 4 && cycle < 2 && name == "shared-0")
            {
                continue;
            }
            let file = if name.starts_with("hidden") {
                root.join("hidden")
            } else {
                root.clone()
            }
            .join(format!("{name}.JPG"));
            assert_eq!(std::fs::read(&file).unwrap(), MEDIA);
            let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
            if let Some((_, prior)) = timestamps.iter().find(|(path, _)| path == &file) {
                assert_eq!(&stamp, prior);
            } else {
                timestamps.push((file, stamp));
            }
        }
        assert_eq!(
            db.get_metadata(&sync_token_key(SHARED)).await.unwrap(),
            Some(format!("current-after-{cycle}"))
        );
        if cycle > 1 {
            assert!(
                probes
                    .iter()
                    .all(|probe| probe.queries.load(Ordering::SeqCst) == 0)
            );
        }
        if cycle > 0 {
            assert!(
                db.sparse_identities(PRIMARY).await.unwrap().is_empty()
                    != (mode == 2 || mode == 3 || mode == 5 || (mode == 4 && cycle < 3)),
                "fresh linked target coverage settles only current source work: mode={mode} cycle={cycle}"
            );
            assert_eq!(
                db.get_master_record_name_for_asset(PRIMARY, "asset-primary")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("primary"),
                "current target coverage must not reassign historical Primary identity"
            );
        }
        source_cursors.push(db.get_metadata(&sync_token_key(PRIMARY)).await.unwrap());
    }
    if mode == 4 {
        assert_eq!(
            pending_successes.load(Ordering::SeqCst),
            1,
            "retained destination downloads successfully exactly once"
        );
    }
    server.verify().await;
    assert_eq!(
        source_cursors,
        (0..5)
            .map(|cycle| Some(format!(
                "current-after-{}",
                if mode == 2 || mode == 3 || mode == 5 || (mode == 4 && cycle < 3) {
                    0
                } else {
                    cycle
                }
            )))
            .collect::<Vec<_>>(),
        "independently covered files and durable source retries must not demand attribution"
    );
}

#[tokio::test]
async fn current_coverage_hidden_target_uses_its_selected_pass() {
    coverage_case(1).await;
}
#[tokio::test]
async fn current_coverage_foreign_owner_stays_pending() {
    coverage_case(2).await;
}
#[tokio::test]
async fn current_coverage_unselected_hidden_stays_pending() {
    coverage_case(3).await;
}
#[tokio::test]
async fn current_coverage_pending_target_does_not_hold_healthy_library_and_reopens() {
    coverage_case(4).await;
}

#[tokio::test]
async fn current_coverage_missing_companion_never_claims_complete() {
    coverage_case(5).await;
}
