//! Real cycle proof: selected sparse target uses the ordinary download pipeline.
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
use crate::state::SparseIdentityStore as _;
use crate::sync_loop::test_support::{
    RunCycleDownloadConfigOptions, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state_with_album, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const HIDDEN_LIST: &str = "CPLAssetAndMasterHiddenByAssetDate";
const HIDDEN_OBJ: &str = "CPLAssetHiddenByAssetDate";
const ALBUM_LIST: &str = "CPLContainerRelationLiveByAssetDate";
const ALBUM_OBJ: &str = "CPLContainerRelationNotDeletedByAssetDate:fixture-album";

#[derive(Clone, Debug)]
struct ScanPass {
    list_type: &'static str,
    obj_type: &'static str,
    records: Vec<Value>,
}

#[derive(Clone, Debug)]
struct Session {
    source: bool,
    owner: &'static str,
    quiet: bool,
    unrelated: bool,
    records: Vec<Value>,
    /// Scoped selection answers each ordinary query by its record type instead
    /// of returning one unrestricted page.
    scan: Option<Vec<ScanPass>>,
    /// Current identity lookup is separate evidence from the selected scan.
    lookup_records: Option<Vec<Value>>,
    scan_queries: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    /// A source-side lookup is the first point after the target scan where the
    /// fixture can alter a selected copy without racing the scan itself.
    remove_on_lookup: Option<Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>,
    link_owner: &'static str,
    target_calls: Arc<AtomicUsize>,
    source_calls: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}

impl Session {
    fn count_for(&self, entry: &Value) -> u64 {
        if let Some(scan) = &self.scan {
            let obj_type = entry
                .pointer("/query/filterBy/fieldValue/value/0")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return scan
                .iter()
                .find(|pass| pass.obj_type == obj_type)
                .map_or(0, |pass| u64::from(!pass.records.is_empty()));
        }
        u64::from(!self.source && self.owner == "owner-test" && !self.records.is_empty())
    }

    fn query_records(&self, request: &Value) -> Vec<Value> {
        let offset = request["query"]["filterBy"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|filter| filter["fieldName"] == "startRank")
            .and_then(|filter| filter["fieldValue"]["value"].as_u64())
            .unwrap_or(0);
        if self.source || offset != 0 {
            return Vec::new();
        }
        if request["zoneID"]["ownerRecordName"].as_str() != Some(self.owner) {
            return Vec::new();
        }
        if let Some(scan) = &self.scan {
            let list_type = request["query"]["recordType"].as_str().unwrap_or_default();
            if let Some(queries) = &self.scan_queries {
                queries.lock().unwrap().push(list_type.to_owned());
            }
            return scan
                .iter()
                .find(|pass| pass.list_type == list_type)
                .map_or_else(Vec::new, |pass| pass.records.clone());
        }
        if self.owner == "owner-test" {
            self.records.clone()
        } else {
            Vec::new()
        }
    }

    fn lookup_response(&self, request: &Value) -> Value {
        if self.source {
            if let Some(path) = self
                .remove_on_lookup
                .as_ref()
                .and_then(|slot| slot.lock().unwrap().take())
            {
                std::fs::remove_file(path).unwrap();
            }
            let records: Vec<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| {
                    if id["recordName"] == "unrelated-source" {
                        unrelated_sparse()
                    } else {
                        sparse_for_owner(self.link_owner)
                    }
                })
                .collect();
            return json!({"records":records});
        }
        self.target_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request["zoneID"]["ownerRecordName"], self.owner);
        let requested = request["records"].as_array().unwrap();
        let available = self.lookup_records.as_ref().unwrap_or(&self.records);
        let records: Vec<_> = available
            .iter()
            .filter(|record| {
                requested
                    .iter()
                    .any(|id| id["recordName"] == record["recordName"])
            })
            .cloned()
            .collect();
        json!({"records":records})
    }
}

fn sparse() -> Value {
    sparse_for_owner("owner-test")
}

fn sparse_for_owner(owner: &str) -> Value {
    let mut row = crate::test_helpers::sparse_shared_asset_record();
    row["recordName"] = json!("source-child");
    row["fields"]["linkedShareRecordName"]["value"] = json!("asset-target");
    row["fields"]["linkedShareZoneName"]["value"] = json!("SharedSync-test");
    row["fields"]["linkedShareZoneOwner"]["value"]["recordName"] = json!(owner);
    row
}

fn unrelated_sparse() -> Value {
    let mut row = sparse();
    row["recordName"] = json!("unrelated-source");
    row["fields"]["linkedShareZoneName"]["value"] = json!("SharedSync-unselected");
    row
}

/// One current asset pair with an exact child identity, optionally hidden.
fn target_pair(
    zone: &str,
    child: &str,
    master: &str,
    sync_token: &str,
    hidden: bool,
    download_url: &str,
    size: u64,
    checksum: &str,
) -> Vec<Value> {
    let mut page =
        full_album_page_with_download(zone, master, sync_token, download_url, size, checksum);
    {
        let records = page["records"].as_array_mut().unwrap();
        records[1]["recordName"] = json!(child);
        if hidden {
            records[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
        }
    }
    page["records"].as_array().unwrap().clone()
}

fn media_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn visit(root: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, out);
                } else if path.extension().is_none_or(|extension| extension != "xmp") {
                    out.push(path);
                }
            }
        }
    }
    let mut files = Vec::new();
    visit(root, &mut files);
    files
}

fn target_gets(requests: &[wiremock::Request]) -> usize {
    requests
        .iter()
        .filter(|request| request.url.path() == "/target.jpg")
        .count()
}

#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/internal/records/query/batch?") {
            let batch = request["batch"].as_array().cloned().unwrap_or_default();
            let batch: Vec<Value> = batch
                .iter()
                .map(|entry| {
                    json!({"records":[{"fields":{"itemCount":{"value":self.count_for(entry)}}}]})
                })
                .collect();
            return Ok(json!({ "batch": batch }));
        }
        if url.contains("/records/query?") {
            return Ok(json!({
                "records": self.query_records(&request),
                "syncToken": "inventory-only"
            }));
        }
        if url.contains("/records/lookup?") {
            return Ok(self.lookup_response(&request));
        }
        assert!(url.contains("/changes/zone?"), "unexpected request {url}");
        let zone = if self.source {
            "PrimarySync"
        } else {
            "SharedSync-test"
        };
        if self.source {
            self.source_calls
                .lock()
                .unwrap()
                .push(request["zones"][0]["syncToken"].as_str().map(str::to_owned));
        }
        let records = if self.source && !self.quiet {
            let mut records = vec![sparse_for_owner(self.link_owner)];
            if self.unrelated {
                records.push(unrelated_sparse());
            }
            records
        } else {
            Vec::new()
        };
        Ok(
            json!({"zones":[{"zoneID":{"zoneName":zone,"ownerRecordName":self.owner},
            "syncToken":if self.source {"source-after"} else {"target-after"},
            "moreComing":false,"records":records}]}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Copy)]
struct AlbumSpec {
    name: &'static str,
    list_type: &'static str,
    obj_type: &'static str,
    container_id: Option<&'static str>,
    query_filter_parent: Option<&'static str>,
}

impl AlbumSpec {
    const fn unfiled() -> Self {
        Self {
            name: "All Photos",
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted",
            container_id: None,
            query_filter_parent: None,
        }
    }

    const fn hidden() -> Self {
        Self {
            name: "Hidden",
            list_type: HIDDEN_LIST,
            obj_type: HIDDEN_OBJ,
            container_id: None,
            query_filter_parent: None,
        }
    }

    const fn named_album() -> Self {
        Self {
            name: "Fixture Album",
            list_type: ALBUM_LIST,
            obj_type: ALBUM_OBJ,
            container_id: Some("fixture-album"),
            query_filter_parent: Some("fixture-album"),
        }
    }
}

fn album(
    db: &Arc<state::SqliteStateDb>,
    zone: &str,
    owner: &str,
    spec: AlbumSpec,
    session: Session,
) -> PhotoAlbum {
    let account = state::db::account::AccountOwner::authenticated(
        "runner@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"runner-fixture"}})).unwrap(),
    )
    .unwrap();
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(spec.name),
            list_type: Arc::from(spec.list_type),
            obj_type: Arc::from(spec.obj_type),
            query_filter: spec.query_filter_parent.map(|parent| {
                Arc::new(json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":parent}}]))
            }),
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":zone,"ownerRecordName":owner})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: spec.container_id.map(Arc::from),
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), account, "com"),
        Arc::from("private"),
    );
    album
}

#[tokio::test]
async fn sparse_target_selected_materializes_and_reopened_cycle_is_quiet() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    for (selected, owner_matches, pending, unrelated) in [
        (true, true, false, false),
        (false, true, false, false),
        (true, false, false, false),
        (true, true, true, false),
        (true, true, false, true),
        (true, true, true, true),
    ] {
        let complete = selected && owner_matches && !unrelated;
        // A held source obligation beside eligible target work still reopens so
        // the next cycle can prove that the completed transfer is not repeated.
        let cycles = if complete || (pending && unrelated) {
            2
        } else {
            1
        };
        let server = crate::start_wiremock_or_skip!();
        let bytes = [
            0xff, 0xd8, 0xff, 0xe0, 0, 0x10, 0x4a, 0x46, 0x49, 0x46, 0, 1, 0, 0, 1, 0, 1, 0, 0,
            0xff, 0xd9,
        ];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
        Mock::given(method("GET"))
            .and(path("/target.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(if selected && owner_matches { 1 } else { 0 })
            .mount(&server)
            .await;
        let records = full_album_page_with_download(
            "SharedSync-test",
            "target",
            "target-after",
            &format!("{}/target.jpg", server.uri()),
            bytes.len() as u64,
            &checksum,
        )["records"]
            .as_array()
            .unwrap()
            .clone();
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.db");
        let media = directory.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let config = make_run_cycle_config();
        let enum_hash = download::compute_config_hash(&config);
        let calls = Arc::new(AtomicUsize::new(0));
        let source_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut previous_calls = None;
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        for cycle in 0..cycles {
            let source_calls_at_start = source_calls.lock().unwrap().len();
            let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
            let db: Arc<dyn download::DownloadStore> = inner.clone();
            let builder = make_run_cycle_download_config_builder(&media, db.clone());
            if cycle == 0 {
                let run = inner.start_sync_run().await.unwrap();
                inner
                    .complete_sync_run(run, &state::SyncRunStats::default())
                    .await
                    .unwrap();
                inner
                    .set_metadata(
                        crate::sync_cycle::ENUM_CONFIG_HASH_KEY,
                        if pending { "old-enum" } else { &enum_hash },
                    )
                    .await
                    .unwrap();
                let target_config = builder(
                    download::SyncMode::Full,
                    Arc::new(rustc_hash::FxHashSet::default()),
                    Arc::new(download::AssetGroupings::default()),
                    Arc::from("PrimarySync"),
                );
                inner
                    .set_metadata(
                        download::DOWNLOAD_CONFIG_HASH_KEY,
                        &download::hash_download_config(&target_config),
                    )
                    .await
                    .unwrap();
                for zone in ["PrimarySync", "SharedSync-test"] {
                    inner
                        .set_metadata(&crate::sync_cycle::sync_token_key(zone), "before")
                        .await
                        .unwrap();
                }
            }
            let primary = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                album(
                    &inner,
                    "PrimarySync",
                    "_defaultOwner",
                    AlbumSpec::unfiled(),
                    Session {
                        source: true,
                        owner: "_defaultOwner",
                        quiet: cycle > 0,
                        unrelated,
                        records: Vec::new(),
                        scan: None,
                        lookup_records: None,
                        scan_queries: None,
                        remove_on_lookup: None,
                        link_owner: "owner-test",
                        target_calls: calls.clone(),
                        source_calls: source_calls.clone(),
                    },
                ),
            );
            let shared = make_run_cycle_library_state_with_album(
                "SharedSync-test",
                "sync_token:SharedSync-test",
                album(
                    &inner,
                    "SharedSync-test",
                    if owner_matches {
                        "owner-test"
                    } else {
                        "wrong-owner"
                    },
                    AlbumSpec::unfiled(),
                    Session {
                        source: false,
                        owner: if owner_matches {
                            "owner-test"
                        } else {
                            "wrong-owner"
                        },
                        quiet: true,
                        unrelated: false,
                        records: records.clone(),
                        scan: None,
                        lookup_records: None,
                        scan_queries: None,
                        remove_on_lookup: None,
                        link_owner: "owner-test",
                        target_calls: calls.clone(),
                        source_calls: source_calls.clone(),
                    },
                ),
            );
            let libraries = if selected {
                vec![&primary, &shared]
            } else {
                vec![&primary]
            };
            let result = crate::sync_cycle::run_cycle(
                &libraries,
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                inner
                    .get_all_asset_albums("SharedSync-test")
                    .await
                    .unwrap()
                    .is_empty(),
                "full-library recovery must not invent album membership"
            );
            if complete {
                assert!(
                    !result.stats.identity_incomplete,
                    "sufficient target proof must finish: {result:?}, target calls {}",
                    calls.load(Ordering::SeqCst)
                );
                assert_eq!(
                    result.stats.downloaded,
                    usize::from(cycle == 0),
                    "selected={selected} owner_matches={owner_matches} pending={pending} cycle={cycle}: {result:?}; source={:?}; target_calls={}",
                    source_calls.lock().unwrap(),
                    calls.load(Ordering::SeqCst)
                );
                assert_eq!(
                    inner
                        .get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("source-after")
                );
                assert!(
                    inner
                        .sparse_identities("PrimarySync")
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(
                    inner
                        .get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(download::compute_config_hash(&config).as_str())
                );
                assert!(
                    inner
                        .get_metadata(crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    4,
                    "two targeted identity reads plus two current-selection confirmation reads; quiet reopen must not repeat either"
                );
            } else {
                assert!(result.stats.identity_incomplete);
                assert_eq!(
                    result.stats.downloaded,
                    usize::from(selected && owner_matches && cycle == 0),
                    "eligible target work progresses once, then reopens quietly"
                );
                assert_eq!(
                    inner
                        .get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("before")
                );
                assert_eq!(
                    inner.sparse_identities("PrimarySync").await.unwrap().len(),
                    if unrelated { 2 } else { 1 }
                );
                if pending {
                    assert_eq!(
                        inner
                            .get_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY)
                            .await
                            .unwrap()
                            .as_deref(),
                        Some("old-enum"),
                        "held source work must not activate the pending configuration"
                    );
                    assert_eq!(
                        inner
                            .get_metadata(crate::sync_cycle::PENDING_ENUM_CONFIG_HASH_KEY)
                            .await
                            .unwrap()
                            .as_deref(),
                        Some(enum_hash.as_str())
                    );
                }
                if pending && unrelated {
                    let observed = source_calls.lock().unwrap();
                    let cycle_calls = &observed[source_calls_at_start..];
                    assert!(
                        cycle_calls
                            .iter()
                            .any(|cursor| cursor.as_deref() == Some("before")),
                        "eligible work still requires the retained-cursor bridge: {cycle_calls:?}"
                    );
                    assert!(
                        !cycle_calls
                            .iter()
                            .any(|cursor| cursor.as_deref() == Some("source-after")),
                        "unproven source progress must never be replayed as active: {cycle_calls:?}"
                    );
                }
                let observed_calls = calls.load(Ordering::SeqCst);
                if cycle == 0 {
                    assert_eq!(
                        observed_calls,
                        if selected && owner_matches { 4 } else { 0 },
                        "identity and current-selection confirmation require the exact selected owner"
                    );
                } else {
                    assert_eq!(
                        observed_calls,
                        previous_calls.unwrap(),
                        "reopen must not repeat the completed target lookup"
                    );
                }
                previous_calls = Some(observed_calls);
            }
        }
        server.verify().await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopedPlan {
    Hidden,
    AlbumOnly,
    HiddenAndAlbum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopedProof {
    /// The exact target is observed by the selected scan under its true master.
    Present,
    /// The lookup succeeds, but the selected scan never observes the child.
    AbsentFromScan,
    /// The scan observes one master while the current lookup reports another.
    AmbiguousMaster,
    /// The sparse link names an owner that is not the selected Shared owner.
    WrongOwner,
    /// One of two selected copies disappears after the target scan.
    MissingAdditionalCopy,
}

impl ScopedProof {
    fn link_owner(self) -> &'static str {
        match self {
            Self::WrongOwner => "wrong-owner",
            _ => "owner-test",
        }
    }
}

fn scoped_shared_state(
    db: &Arc<state::SqliteStateDb>,
    plan: ScopedPlan,
    session: Session,
) -> crate::sync_cycle::LibraryState {
    let mut passes = Vec::new();
    if plan != ScopedPlan::AlbumOnly {
        passes.push(crate::commands::AlbumPass {
            kind: crate::commands::PassKind::SmartFolder,
            album: album(
                db,
                "SharedSync-test",
                "owner-test",
                AlbumSpec::hidden(),
                session.clone(),
            ),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        });
    }
    if plan != ScopedPlan::Hidden {
        passes.push(crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: album(
                db,
                "SharedSync-test",
                "owner-test",
                AlbumSpec::named_album(),
                session,
            ),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        });
    }
    make_run_cycle_library_state_with_passes(
        "SharedSync-test",
        "sync_token:SharedSync-test",
        passes,
    )
}

fn plan_scan_passes(plan: ScopedPlan, records: Vec<Value>) -> Vec<ScanPass> {
    let mut passes = Vec::new();
    if plan != ScopedPlan::AlbumOnly {
        passes.push(ScanPass {
            list_type: HIDDEN_LIST,
            obj_type: HIDDEN_OBJ,
            records: records.clone(),
        });
    }
    if plan != ScopedPlan::Hidden {
        passes.push(ScanPass {
            list_type: ALBUM_LIST,
            obj_type: ALBUM_OBJ,
            records,
        });
    }
    passes
}

#[tokio::test]
async fn sparse_scoped_hidden_and_album_scan_proves_only_the_exact_shared_target() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    for plan in [
        ScopedPlan::Hidden,
        ScopedPlan::AlbumOnly,
        ScopedPlan::HiddenAndAlbum,
    ] {
        for shared_first in [false, true] {
            let proofs: &[ScopedProof] = if plan == ScopedPlan::HiddenAndAlbum {
                &[
                    ScopedProof::Present,
                    ScopedProof::AbsentFromScan,
                    ScopedProof::AmbiguousMaster,
                    ScopedProof::WrongOwner,
                    ScopedProof::MissingAdditionalCopy,
                ]
            } else {
                &[
                    ScopedProof::Present,
                    ScopedProof::AbsentFromScan,
                    ScopedProof::AmbiguousMaster,
                    ScopedProof::WrongOwner,
                ]
            };
            for &proof in proofs {
                let server = crate::start_wiremock_or_skip!();
                let bytes = [
                    0xff, 0xd8, 0xff, 0xe0, 0, 0x10, 0x4a, 0x46, 0x49, 0x46, 0, 1, 0, 0, 1, 0, 1,
                    0, 0, 0xff, 0xd9,
                ];
                let checksum =
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
                let expected_copies = usize::from(plan == ScopedPlan::HiddenAndAlbum) + 1;
                // The same scoped target-only plan establishes every required
                // copy before the due source obligation is observed.
                Mock::given(method("GET"))
                    .and(path("/target.jpg"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_bytes(bytes)
                            .insert_header("content-type", "image/jpeg"),
                    )
                    .expect(expected_copies as u64)
                    .mount(&server)
                    .await;
                let url = format!("{}/target.jpg", server.uri());
                let target = || {
                    target_pair(
                        "SharedSync-test",
                        "asset-target",
                        "target",
                        "target-after",
                        true,
                        &url,
                        bytes.len() as u64,
                        &checksum,
                    )
                };
                let scanned = if proof == ScopedProof::AbsentFromScan {
                    Vec::new()
                } else {
                    target()
                };
                let looked_up = if proof == ScopedProof::AmbiguousMaster {
                    target_pair(
                        "SharedSync-test",
                        "asset-target",
                        "other-master",
                        "target-after",
                        true,
                        &url,
                        bytes.len() as u64,
                        &checksum,
                    )
                } else {
                    target()
                };
                let directory = tempfile::tempdir().unwrap();
                let database = directory.path().join("state.db");
                let media = directory.path().join("media");
                std::fs::create_dir(&media).unwrap();
                let config = make_run_cycle_config();
                let enum_hash = download::compute_config_hash(&config);
                let calls = Arc::new(AtomicUsize::new(0));
                let target_calls = Arc::new(AtomicUsize::new(0));
                let scan_queries = Arc::new(std::sync::Mutex::new(Vec::new()));
                let source_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
                let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
                let inner = crate::sync_loop::test_support::open_owned_cycle_db(&database).await;
                let db: Arc<dyn download::DownloadStore> = inner.clone();
                let builder = make_run_cycle_download_config_builder_with_options(
                    &media,
                    db.clone(),
                    RunCycleDownloadConfigOptions {
                        per_pass_paths: true,
                        ..Default::default()
                    },
                );
                let run = inner.start_sync_run().await.unwrap();
                inner
                    .complete_sync_run(run, &state::SyncRunStats::default())
                    .await
                    .unwrap();
                inner
                    .set_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY, &enum_hash)
                    .await
                    .unwrap();
                let seed_hash = |zone: &str| {
                    let probe = builder(
                        download::SyncMode::Full,
                        Arc::new(rustc_hash::FxHashSet::default()),
                        Arc::new(download::AssetGroupings::default()),
                        Arc::from(zone),
                    );
                    download::hash_download_config(&probe)
                };
                inner
                    .set_metadata(
                        download::DOWNLOAD_CONFIG_HASH_KEY,
                        &seed_hash("SharedSync-test"),
                    )
                    .await
                    .unwrap();
                // Seed through the same scoped plan. A mixed plan owns two
                // distinct selected paths and therefore seeds two copies.
                let prior_shared = scoped_shared_state(
                    &inner,
                    plan,
                    Session {
                        source: false,
                        owner: "owner-test",
                        quiet: true,
                        unrelated: false,
                        records: Vec::new(),
                        scan: Some(plan_scan_passes(plan, target())),
                        lookup_records: Some(target()),
                        scan_queries: None,
                        remove_on_lookup: None,
                        link_owner: "owner-test",
                        target_calls: calls.clone(),
                        source_calls: source_calls.clone(),
                    },
                );
                let prior = crate::sync_cycle::run_cycle(
                    &[&prior_shared],
                    &config,
                    Some(db.as_ref()),
                    &builder,
                    download::DownloadControls::download_hidden(),
                    &shared_session,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
                assert_eq!(
                    prior.stats.downloaded, expected_copies,
                    "the prior scoped scan must establish every required copy: plan={plan:?} proof={proof:?}: {prior:?}"
                );
                let seeded_requests = server.received_requests().await.unwrap_or_default();
                assert_eq!(
                    target_gets(&seeded_requests),
                    expected_copies,
                    "seeding must transfer each distinct selected path once: plan={plan:?} proof={proof:?}"
                );
                let mut seeded_files = media_files(&media);
                seeded_files.sort();
                assert_eq!(
                    seeded_files.len(),
                    expected_copies,
                    "seeding must leave one file per selected path: plan={plan:?} proof={proof:?} files={seeded_files:?}"
                );
                for path in &seeded_files {
                    assert_eq!(
                        std::fs::read(path).unwrap().as_slice(),
                        bytes.as_slice(),
                        "seeded copy changed: {path:?}"
                    );
                }
                let canonical = if plan == ScopedPlan::HiddenAndAlbum {
                    let conn = rusqlite::Connection::open(&database).unwrap();
                    Some(
                        conn.query_row(
                            "SELECT local_path FROM assets WHERE library='SharedSync-test' AND id='asset-target' AND local_path IS NOT NULL",
                            [],
                            |row| row.get::<_, String>(0),
                        )
                        .unwrap(),
                    )
                } else {
                    None
                };
                let removed_copy = if proof == ScopedProof::MissingAdditionalCopy {
                    let canonical = canonical
                        .as_deref()
                        .expect("mixed plan has a canonical path");
                    let additional = seeded_files
                        .iter()
                        .find(|path| path.to_string_lossy() != canonical)
                        .cloned()
                        .expect("mixed plan seeds a noncanonical copy");
                    Some(additional)
                } else {
                    None
                };
                let remove_on_lookup = removed_copy
                    .clone()
                    .map(|path| Arc::new(std::sync::Mutex::new(Some(path))));
                // Retain a due Primary obligation. Its observed evidence is the
                // only reason an Album-only cycle must refresh its sealed root.
                let sparse_fields = sparse_for_owner(proof.link_owner())["fields"].clone();
                let evidence =
                    crate::icloud::photos::asset::SparseShareEvidence::from_fields(&sparse_fields)
                        .expect("sparse fixture is linked evidence")
                        .durable_key()
                        .expect("sparse fixture has a durable key");
                let past = chrono::Utc::now() - chrono::TimeDelta::minutes(5);
                inner
                    .observe_sparse_identity(
                        "PrimarySync",
                        &state::SparseSourceId::new("source-child"),
                        &evidence,
                        past,
                    )
                    .await
                    .unwrap();
                inner
                    .set_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"), "before")
                    .await
                    .unwrap();
                let first_zone = if shared_first {
                    "SharedSync-test"
                } else {
                    "PrimarySync"
                };
                inner
                    .set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, &seed_hash(first_zone))
                    .await
                    .unwrap();
                let primary = make_run_cycle_library_state_with_album(
                    "PrimarySync",
                    "sync_token:PrimarySync",
                    album(
                        &inner,
                        "PrimarySync",
                        "_defaultOwner",
                        AlbumSpec::unfiled(),
                        Session {
                            source: true,
                            owner: "_defaultOwner",
                            quiet: true,
                            unrelated: false,
                            records: Vec::new(),
                            scan: None,
                            lookup_records: None,
                            scan_queries: None,
                            remove_on_lookup: remove_on_lookup.clone(),
                            link_owner: proof.link_owner(),
                            target_calls: calls.clone(),
                            source_calls: source_calls.clone(),
                        },
                    ),
                );
                let shared = scoped_shared_state(
                    &inner,
                    plan,
                    Session {
                        source: false,
                        owner: "owner-test",
                        quiet: true,
                        unrelated: false,
                        records: Vec::new(),
                        scan: Some(plan_scan_passes(plan, scanned)),
                        lookup_records: Some(looked_up),
                        scan_queries: Some(scan_queries.clone()),
                        remove_on_lookup: None,
                        link_owner: proof.link_owner(),
                        target_calls: target_calls.clone(),
                        source_calls: source_calls.clone(),
                    },
                );
                let libraries = if shared_first {
                    vec![&shared, &primary]
                } else {
                    vec![&primary, &shared]
                };
                let result = crate::sync_cycle::run_cycle(
                    &libraries,
                    &config,
                    Some(db.as_ref()),
                    &builder,
                    download::DownloadControls::download_hidden(),
                    &shared_session,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
                let queried = scan_queries.lock().unwrap().clone();
                if plan != ScopedPlan::AlbumOnly {
                    assert!(
                        queried.iter().any(|list_type| list_type == HIDDEN_LIST),
                        "the Hidden query must be re-read: plan={plan:?} queried={queried:?}"
                    );
                }
                if plan != ScopedPlan::Hidden && proof != ScopedProof::WrongOwner {
                    assert!(
                        queried.iter().any(|list_type| list_type == ALBUM_LIST),
                        "the named album must be re-read for the due sparse source: plan={plan:?} proof={proof:?} queried={queried:?}"
                    );
                }
                let requests = server.received_requests().await.unwrap_or_default();
                assert_eq!(
                    target_gets(&requests),
                    expected_copies,
                    "recovery must add zero media GETs after seeding: plan={plan:?} shared_first={shared_first} proof={proof:?}"
                );
                let files_after = media_files(&media);
                if let Some(removed) = &removed_copy {
                    assert!(
                        remove_on_lookup
                            .as_ref()
                            .is_some_and(|slot| slot.lock().unwrap().is_none()),
                        "the source hook must run exactly once"
                    );
                    assert!(!removed.exists(), "the additional copy must stay removed");
                    assert_eq!(files_after.len(), expected_copies - 1);
                    let canonical = canonical
                        .as_deref()
                        .expect("mixed plan has a canonical path");
                    let canonical = std::path::Path::new(canonical);
                    assert!(
                        canonical.exists(),
                        "the canonical copy must remain intact: {canonical:?}"
                    );
                    assert_eq!(
                        std::fs::read(canonical).unwrap().as_slice(),
                        bytes.as_slice()
                    );
                } else {
                    assert_eq!(files_after.len(), expected_copies);
                    for path in &seeded_files {
                        assert_eq!(
                            std::fs::read(path).unwrap().as_slice(),
                            bytes.as_slice(),
                            "selected copy changed during recovery: {path:?}"
                        );
                    }
                }
                assert_eq!(
                    result.stats.downloaded,
                    0,
                    "verified-copy recovery must not issue a new media GET: plan={plan:?} shared_first={shared_first} proof={proof:?}: {result:?}; target_calls={}",
                    target_calls.load(Ordering::SeqCst)
                );
                let identities = inner.sparse_identities("PrimarySync").await.unwrap();
                if proof == ScopedProof::Present {
                    assert!(
                        !result.stats.identity_incomplete,
                        "an exact selected scan must finish the source proof: {result:?}; target_calls={}",
                        target_calls.load(Ordering::SeqCst)
                    );
                    assert_eq!(
                        inner
                            .get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some("source-after")
                    );
                    assert!(identities.is_empty());
                    assert!(
                        target_calls.load(Ordering::SeqCst) >= 2,
                        "the exact target still requires a real identity lookup"
                    );
                } else {
                    assert!(
                        result.stats.identity_incomplete,
                        "insufficient proof must hold the source checkpoint: {result:?}"
                    );
                    assert_eq!(
                        inner
                            .get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some("before")
                    );
                    assert_eq!(identities.len(), 1);
                    if proof != ScopedProof::WrongOwner {
                        assert!(
                            target_calls.load(Ordering::SeqCst) >= 2,
                            "the negative case still performed a successful lookup"
                        );
                    }
                }
                server.verify().await;
            }
        }
    }
}
