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
        let records = if self.source && !self.quiet {
            let mut records = vec![sparse()];
            if self.unrelated {
                records.push(unrelated_sparse());
            }
            records
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
                    "PrimarySync",
                    "_defaultOwner",
                    Session {
                        source: true,
                        quiet: cycle > 0,
                        unrelated,
                        records: Vec::new(),
                        target_calls: calls.clone(),
                        source_calls: source_calls.clone(),
                    },
                ),
            );
            let shared = make_run_cycle_library_state_with_album(
                "SharedSync-test",
                "sync_token:SharedSync-test",
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
                    2,
                    "quiet reopened cycle must not lookup target again"
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

#[derive(Clone, Debug)]
struct RelationOnlySession(Session);
#[async_trait::async_trait]
impl PhotosSession for RelationOnlySession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if self.0.source && url.contains("/changes/zone?") {
            let request: Value = serde_json::from_str(&body)?;
            let records = if request["zones"][0]["syncToken"] == "before" {
                vec![
                    json!({"recordName":"source-child-IN-vacation","recordType":"CPLContainerRelation",
                    "fields":{"containerId":{"value":"vacation"},"itemId":{"value":"source-child"}}}),
                ]
            } else {
                Vec::new()
            };
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                "syncToken":"source-after","moreComing":false,"records":records}]}),
            );
        }
        self.0.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn relation_only_sparse_discovery_reopens_and_recovers_selected_target_once() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };
    let server = crate::start_wiremock_or_skip!();
    let bytes = [
        0xff, 0xd8, 0xff, 0xe0, 0, 0x10, 0x4a, 0x46, 0x49, 0x46, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0xff,
        0xd9,
    ];
    Mock::given(method("GET"))
        .and(path("/relation.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    let records = full_album_page_with_download(
        "SharedSync-test",
        "target",
        "target-after",
        &format!("{}/relation.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let database = root.path().join("state.db");
    let mut config = make_run_cycle_config();
    config.filters.selection.albums = crate::selection::AlbumSelector::Named {
        included: std::collections::BTreeSet::from(["Vacation".into()]),
        excluded: Default::default(),
    };
    let enum_hash = download::compute_config_hash(&config);
    let calls = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    for cycle in 0..3 {
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
                .set_metadata(crate::sync_cycle::ENUM_CONFIG_HASH_KEY, &enum_hash)
                .await
                .unwrap();
            let cfg = builder(
                download::SyncMode::Full,
                Arc::default(),
                Arc::new(download::AssetGroupings::default()),
                Arc::from("PrimarySync"),
            );
            inner
                .set_metadata(
                    download::DOWNLOAD_CONFIG_HASH_KEY,
                    &download::hash_download_config(&cfg),
                )
                .await
                .unwrap();
            for zone in ["PrimarySync", "SharedSync-test"] {
                inner
                    .set_metadata(&crate::sync_cycle::sync_token_key(zone), "before")
                    .await
                    .unwrap();
            }
            inner
                .upsert_album_container("PrimarySync", "vacation", "Vacation", "album")
                .await
                .unwrap();
            let generation = inner
                .start_album_membership_snapshot("PrimarySync", "vacation", Some("hash-test"))
                .await
                .unwrap();
            inner
                .complete_album_membership_snapshot("PrimarySync", "vacation", generation)
                .await
                .unwrap();
        }
        let provider = RelationOnlySession(Session {
            source: true,
            quiet: true,
            unrelated: false,
            records: Vec::new(),
            target_calls: calls.clone(),
            source_calls: Arc::default(),
        });
        let selected_album = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("Vacation"),
                list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
                obj_type: Arc::from("CPLAssetInContainerByAssetDate"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: Some(Arc::from("vacation")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider.clone()),
        );
        let mut primary = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            selected_album,
        );
        primary.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(provider),
            "PrimarySync",
        );
        primary.pass_scope.include_unfiled = false;
        primary.pass_scope.include_albums = true;
        primary.plan.passes[0].kind = crate::commands::PassKind::Album;
        let target_provider = Session {
            source: false,
            quiet: true,
            unrelated: false,
            records: records.clone(),
            target_calls: calls.clone(),
            source_calls: Arc::default(),
        };
        let mut shared = make_run_cycle_library_state_with_album(
            "SharedSync-test",
            "sync_token:SharedSync-test",
            album("SharedSync-test", "owner-test", target_provider.clone()),
        );
        shared.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(target_provider),
            "SharedSync-test",
        );
        let result = crate::sync_cycle::run_cycle(
            &[&primary, &shared],
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
        assert_eq!(
            result.stats.downloaded,
            usize::from(cycle == 1),
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before"),
            "a recovered target does not prove the unresolved source album membership"
        );
        assert!(
            inner
                .get_all_asset_albums("SharedSync-test")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            inner.sparse_identities("PrimarySync").await.unwrap().len(),
            1,
            "cycle {cycle}"
        );
        if cycle == 0 {
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "completed target should not be looked up repeatedly"
            );
        }
        let rows = inner.get_downloaded_page(0, 10).await.unwrap();
        let targets: Vec<_> = rows
            .iter()
            .filter(|row| row.library.as_ref() == "SharedSync-test")
            .collect();
        if cycle == 0 {
            assert!(targets.is_empty());
        } else {
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].id.as_ref(), "asset-target");
            assert_eq!(targets[0].version_size.as_str(), "original");
            assert_eq!(
                std::fs::read(targets[0].local_path.as_ref().unwrap()).unwrap(),
                bytes
            );
            assert!(matches!(
                db.legacy_child_provenance("SharedSync-test", "asset-target", "original")
                    .await
                    .unwrap(),
                Some(crate::state::db::trusted_legacy::LegacyReceiptProvenance::VerifiedSource)
            ));
        }
    }
    server.verify().await;
}
