//! Real cycle proof: selected sparse target uses the ordinary download pipeline.
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
use crate::state::SparseIdentityStore as _;
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Debug)]
struct Session {
    source: bool,
    quiet: bool,
    unrelated: bool,
    late: bool,
    same_source: bool,
    paired_sent: Arc<AtomicUsize>,
    tamper_root: Option<std::path::PathBuf>,
    tampered: Arc<AtomicUsize>,
    tampered_path: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    records: Vec<Value>,
    target_calls: Arc<AtomicUsize>,
    source_calls: Arc<std::sync::Mutex<Vec<Option<String>>>>,
}
fn sparse() -> Value {
    let mut row = crate::test_helpers::sparse_shared_asset_record();
    row["recordName"] = json!("source-child");
    row["fields"]["linkedShareRecordName"]["value"] = json!("asset-target");
    row["fields"]["linkedShareZoneName"]["value"] = json!("SharedSync-test");
    row["fields"]["linkedShareZoneOwner"]["value"]["recordName"] = json!("owner-test");
    row
}
fn unrelated_sparse() -> Value {
    let mut row = sparse();
    row["recordName"] = json!("unrelated-source");
    row["fields"]["linkedShareZoneName"]["value"] = json!("SharedSync-unselected");
    row
}
#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response(0));
        }
        if url.contains("/records/query?") {
            return Ok(json!({"records":[],"syncToken":"inventory-only"}));
        }
        if url.contains("/records/lookup?") {
            if self.source {
                let records: Vec<_> = request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| {
                        if id["recordName"] == "unrelated-source" {
                            unrelated_sparse()
                        } else {
                            sparse()
                        }
                    })
                    .collect();
                return Ok(json!({"records":records}));
            }
            self.target_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request["zoneID"]["ownerRecordName"], "owner-test");
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|id| id["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
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
        if self.source
            && request["zones"][0]["syncToken"] == "source-after"
            && let Some(root) = &self.tamper_root
            && self.tampered.fetch_add(1, Ordering::SeqCst) == 0
        {
            let mut directories = vec![root.clone()];
            let mut found = Vec::new();
            while let Some(directory) = directories.pop() {
                for entry in std::fs::read_dir(directory)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() {
                        directories.push(entry.path());
                    } else if std::fs::read(entry.path())?
                        == [
                            0xff, 0xd8, 0xff, 0xe0, 0, 0x10, 0x4a, 0x46, 0x49, 0x46, 0, 1, 0, 0, 1,
                            0, 1, 0, 0, 0xff, 0xd9,
                        ]
                    {
                        found.push(entry.path());
                    }
                }
            }
            assert_eq!(
                found.len(),
                1,
                "the completed target must exist before tampering"
            );
            std::fs::write(&found[0], b"synthetic changed target")?;
            *self.tampered_path.lock().unwrap() = Some(found[0].clone());
        }
        let records = if self.source && !self.quiet {
            if request["zones"][0]["syncToken"] == "source-after" {
                if self.late {
                    vec![unrelated_sparse()]
                } else if self.same_source {
                    self.paired_sent.fetch_add(1, Ordering::SeqCst);
                    self.records.clone()
                } else {
                    Vec::new()
                }
            } else {
                let mut records = vec![sparse()];
                if self.unrelated {
                    records.push(unrelated_sparse());
                }
                records
            }
        } else {
            Vec::new()
        };
        Ok(
            json!({"zones":[{"zoneID":{"zoneName":zone,"ownerRecordName":if self.source {"_defaultOwner"} else {"owner-test"}},
            "syncToken":if self.source {"source-after"} else {"target-after"},
            "moreComing":false,"records":records}]}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
fn album(zone: &str, owner: &str, session: Session) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("All Photos"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":zone,"ownerRecordName":owner})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    )
}

#[tokio::test]
#[allow(clippy::print_stderr, reason = "synthetic bridge regression markers")]
async fn sparse_target_selected_materializes_and_reopened_cycle_is_quiet() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    for (selected, owner_matches, pending, unrelated, baseline, late, same_source, tamper) in [
        (true, true, false, false, true, false, false, false),
        (true, true, false, false, true, true, false, false),
        (true, true, false, false, true, false, true, false),
        (true, true, false, false, true, false, false, true),
        (true, true, false, false, false, false, false, false),
        (false, true, false, false, false, false, false, false),
        (true, false, false, false, false, false, false, false),
        (true, true, true, false, false, false, false, false),
        (true, true, false, true, false, false, false, false),
        (true, true, true, true, false, false, false, false),
    ] {
        let complete = selected && owner_matches && !unrelated && !late && !same_source && !tamper;
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
        let mut changed_source = full_album_page_with_download(
            "PrimarySync",
            "changed-master",
            "source-after",
            &format!("{}/target.jpg", server.uri()),
            bytes.len() as u64,
            &checksum,
        )["records"]
            .as_array()
            .unwrap()
            .clone();
        changed_source[0]["fields"]["filenameEnc"] = json!({"value":"changed.jpg","type":"STRING"});
        changed_source[1]["recordName"] = json!("source-child");
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.db");
        let media = directory.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let config = make_run_cycle_config();
        let enum_hash = download::compute_config_hash(&config);
        let calls = Arc::new(AtomicUsize::new(0));
        let paired_sent = Arc::new(AtomicUsize::new(0));
        let tampered = Arc::new(AtomicUsize::new(0));
        let tampered_path = Arc::new(std::sync::Mutex::new(None));
        let source_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut previous_calls = None;
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        for cycle in 0..cycles {
            let source_calls_at_start = source_calls.lock().unwrap().len();
            let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
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
                    if baseline && zone == "PrimarySync" {
                        continue;
                    }
                    inner
                        .set_metadata(&crate::sync_cycle::sync_token_key(zone), "before")
                        .await
                        .unwrap();
                }
            }
            let primary = make_run_cycle_library_state_with_album(
                "PrimarySync",
                &crate::sync_cycle::sync_token_key("PrimarySync"),
                album(
                    "PrimarySync",
                    "_defaultOwner",
                    Session {
                        source: true,
                        quiet: cycle > 0,
                        unrelated,
                        late,
                        same_source,
                        paired_sent: paired_sent.clone(),
                        tamper_root: tamper.then(|| media.clone()),
                        tampered: tampered.clone(),
                        tampered_path: tampered_path.clone(),
                        records: changed_source.clone(),
                        target_calls: calls.clone(),
                        source_calls: source_calls.clone(),
                    },
                ),
            );
            let shared = make_run_cycle_library_state_with_album(
                "SharedSync-test",
                &crate::sync_cycle::sync_token_key("SharedSync-test"),
                album(
                    "SharedSync-test",
                    if owner_matches {
                        "owner-test"
                    } else {
                        "wrong-owner"
                    },
                    Session {
                        source: false,
                        quiet: true,
                        unrelated: false,
                        late: false,
                        same_source: false,
                        paired_sent: paired_sent.clone(),
                        tamper_root: None,
                        tampered: tampered.clone(),
                        tampered_path: tampered_path.clone(),
                        records: records.clone(),
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
            let validation_failures_before =
                crate::test_helpers::SPARSE_TARGET_VALIDATION_FAILURES.load(Ordering::SeqCst);
            let result = crate::sync_cycle::run_cycle(
                &libraries,
                &config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            if baseline && cycle == 0 {
                let calls = source_calls.lock().unwrap();
                assert!(
                    calls
                        .iter()
                        .any(|cursor| cursor.as_deref() == Some("fixture-zone-history-boundary"))
                );
                assert!(
                    calls
                        .iter()
                        .any(|cursor| cursor.as_deref() == Some("source-after")),
                    "the second bridge must actually run"
                );
            }
            if tamper {
                assert_eq!(
                    tampered.load(Ordering::SeqCst),
                    1,
                    "target must be changed after the first bridge"
                );
                assert!(
                    crate::test_helpers::SPARSE_TARGET_VALIDATION_FAILURES.load(Ordering::SeqCst)
                        > validation_failures_before,
                    "the final checkpoint target-file validator must actually reject the changed file"
                );
                let changed = tampered_path.lock().unwrap().clone().unwrap();
                assert_eq!(std::fs::read(changed).unwrap(), b"synthetic changed target");
                assert!(
                    inner
                        .get_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"))
                        .await
                        .unwrap()
                        .is_none(),
                    "changed target must not certify a source checkpoint"
                );
                assert_eq!(
                    inner.sparse_identities("PrimarySync").await.unwrap().len(),
                    1,
                    "changed target retains the sparse obligation"
                );
                assert_eq!(calls.load(Ordering::SeqCst), 2);
                eprintln!("SPARSE_TARGET_MUTATION_REACHED checkpoint_held=1 debt=1");
                continue;
            }
            if same_source {
                assert!(
                    paired_sent.load(Ordering::SeqCst) > 0,
                    "same-source paired change must reach the second provider bridge"
                );
                assert!(
                    result.stats.identity_incomplete,
                    "an intervening paired source cannot reuse the old proof"
                );
            }
            eprintln!(
                "SPARSE_BRIDGE_REACHED selected={selected} owner_matches={owner_matches} pending={pending} unrelated={unrelated} baseline={baseline} late={late} same_source={same_source} cycle={cycle} identity_incomplete={} downloads={}",
                result.stats.identity_incomplete, result.stats.downloaded
            );
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
                assert_eq!(result.stats.downloaded, usize::from(cycle == 0));
                assert_eq!(
                    inner
                        .get_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"))
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
                    2,
                    "quiet reopened cycle must not lookup target again"
                );
            } else {
                assert!(result.stats.identity_incomplete);
                assert_eq!(
                    result.stats.downloaded,
                    usize::from(selected && owner_matches && cycle == 0),
                    "eligible target work progresses while changed source remains held"
                );
                assert_eq!(
                    inner
                        .get_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"))
                        .await
                        .unwrap()
                        .as_deref(),
                    if baseline { None } else { Some("before") }
                );
                assert_eq!(
                    inner.sparse_identities("PrimarySync").await.unwrap().len(),
                    if unrelated || late { 2 } else { 1 }
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
                        if selected && owner_matches { 2 } else { 0 },
                        "unselected or wrong scope must not be requested"
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
