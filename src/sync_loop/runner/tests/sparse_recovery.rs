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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Companion {
    None,
    Hidden,
    HiddenTarget,
    AlbumOnly,
    Filtered,
    ForeignOwner,
    ForeignZone,
    ForeignAccount,
    OtherCollection,
    DuplicateOrdinary,
    DuplicateHidden,
    ConfigChange,
}
fn hidden_companion(case: Companion, session: Session, zone: &str, owner: &str) -> PhotoAlbum {
    let mut params = std::collections::HashMap::new();
    if case == Companion::ForeignAccount {
        params.insert("dsid".into(), json!("different-test-account"));
    }
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(params),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(if case == Companion::OtherCollection {
                "Favorites"
            } else {
                "Hidden"
            }),
            list_type: Arc::from(if case == Companion::OtherCollection {
                "CPLAssetAndMasterInSmartAlbumByAssetDate"
            } else {
                "CPLAssetAndMasterHiddenByAssetDate"
            }),
            obj_type: Arc::from("CPLAssetHiddenByAssetDate"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(
                json!({"zoneName":if case == Companion::ForeignZone {"SharedSync-other"} else {zone},
            "ownerRecordName":if case == Companion::ForeignOwner {"foreign-owner"} else {owner}}),
            ),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    )
}
#[tokio::test]
async fn sparse_target_selected_materializes_and_reopened_cycle_is_quiet() {
    Box::pin(exercise_companion(Companion::None)).await;
}
#[tokio::test]
async fn sparse_hidden_companion_refuses_unsafe_plans_and_hidden_targets() {
    for case in [
        Companion::HiddenTarget,
        Companion::AlbumOnly,
        Companion::Filtered,
        Companion::ForeignOwner,
        Companion::ForeignZone,
        Companion::ForeignAccount,
        Companion::OtherCollection,
        Companion::DuplicateOrdinary,
        Companion::DuplicateHidden,
    ] {
        Box::pin(exercise_companion(case)).await;
    }
}
#[tokio::test]
async fn sparse_hidden_companion_recovers_once_and_preserves_holds_across_configuration_change() {
    for case in [Companion::Hidden, Companion::ConfigChange] {
        Box::pin(exercise_companion(case)).await;
    }
}
async fn exercise_companion(case: Companion) {
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
        if case != Companion::None && (!selected || !owner_matches || !pending || !unrelated) {
            continue;
        }
        let plan_eligible = matches!(
            case,
            Companion::None | Companion::Hidden | Companion::HiddenTarget | Companion::ConfigChange
        );
        let can_recover =
            selected && owner_matches && plan_eligible && case != Companion::HiddenTarget;
        let complete = can_recover && !unrelated;
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
            .expect(if can_recover { 1 } else { 0 })
            .mount(&server)
            .await;
        let mut records = full_album_page_with_download(
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
        if case == Companion::HiddenTarget {
            for record in &mut records {
                if record["recordType"] == "CPLAsset" {
                    record["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.db");
        let media = directory.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let mut config = make_run_cycle_config();
        if case != Companion::None {
            config.filters.selection.libraries =
                crate::selection::parse_library_selector(&["shared".into()]).unwrap();
            config.filters.selection.albums = crate::selection::AlbumSelector::None;
            config.filters.selection.smart_folders = crate::selection::SmartFolderSelector::Named {
                included: std::collections::BTreeSet::from(["Hidden".into()]),
                excluded: std::collections::BTreeSet::new(),
            };
            config.filters.selection.smart_folders_explicit = true;
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let source_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut previous_calls = None;
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        for cycle in 0..cycles {
            if case == Companion::ConfigChange && cycle == 1 {
                config.filters.selection.smart_folders =
                    crate::selection::SmartFolderSelector::Named {
                        included: std::collections::BTreeSet::from([
                            "Hidden".into(),
                            "Favorites".into(),
                        ]),
                        excluded: std::collections::BTreeSet::new(),
                    };
            }
            let enum_hash = download::compute_config_hash(&config);
            let source_calls_at_start = source_calls.lock().unwrap().len();
            let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
            let db: Arc<dyn download::DownloadStore> = inner.clone();
            let base_builder = make_run_cycle_download_config_builder(&media, db.clone());
            let builder = |mode, excluded, groups, library| {
                let mut built = base_builder(mode, excluded, groups, library);
                if case != Companion::None {
                    let built = Arc::make_mut(&mut built);
                    built.folder_structure = "ordinary".into();
                    built.folder_structure_smart_folders = Arc::from("hidden");
                }
                built
            };
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
            let mut primary = make_run_cycle_library_state_with_album(
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
            let mut shared = make_run_cycle_library_state_with_album(
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
            if case != Companion::None {
                let effective = if case == Companion::ConfigChange {
                    Companion::Hidden
                } else {
                    case
                };
                let source = Session {
                    source: true,
                    quiet: cycle > 0,
                    unrelated,
                    records: Vec::new(),
                    target_calls: calls.clone(),
                    source_calls: source_calls.clone(),
                };
                primary.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                    Box::new(source.clone()),
                    "PrimarySync",
                );
                primary.pass_scope.include_unfiled = false;
                primary.pass_scope.include_smart_folders = true;
                primary.plan.passes[0].kind = crate::commands::PassKind::SmartFolder;
                primary.plan.passes[0].album =
                    hidden_companion(Companion::Hidden, source, "PrimarySync", "_defaultOwner");
                let companion = || crate::commands::AlbumPass {
                    kind: crate::commands::PassKind::SmartFolder,
                    album: hidden_companion(
                        effective,
                        Session {
                            source: false,
                            quiet: true,
                            unrelated: false,
                            records: records.clone(),
                            target_calls: calls.clone(),
                            source_calls: source_calls.clone(),
                        },
                        "SharedSync-test",
                        "owner-test",
                    ),
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                };
                shared.plan.passes.push(companion());
                if case == Companion::ConfigChange && cycle == 1 {
                    shared.plan.passes.push(crate::commands::AlbumPass {
                        kind: crate::commands::PassKind::SmartFolder,
                        album: hidden_companion(
                            Companion::OtherCollection,
                            Session {
                                source: false,
                                quiet: true,
                                unrelated: false,
                                records: records.clone(),
                                target_calls: calls.clone(),
                                source_calls: source_calls.clone(),
                            },
                            "SharedSync-test",
                            "owner-test",
                        ),
                        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                    });
                }
                shared.pass_scope.include_smart_folders = true;
                if case == Companion::AlbumOnly {
                    shared.plan.passes[0].kind = crate::commands::PassKind::Album;
                    shared.pass_scope.include_unfiled = false;
                    shared.pass_scope.include_albums = true;
                }
                if case == Companion::Filtered {
                    shared.plan.passes[0].exclude_ids =
                        Arc::new(rustc_hash::FxHashSet::from_iter(["asset-target".into()]));
                }
                if case == Companion::DuplicateOrdinary {
                    shared.plan.passes.push(shared.plan.passes[0].clone());
                }
                if case == Companion::DuplicateHidden {
                    shared.plan.passes.push(companion());
                }
            }
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
            if case != Companion::None {
                let rows = inner.get_downloaded_page(0, 10).await.unwrap();
                let targets: Vec<_> = rows
                    .iter()
                    .filter(|row| row.library.as_ref() == "SharedSync-test")
                    .collect();
                if can_recover {
                    assert_eq!(targets.len(), 1);
                    let row = targets[0];
                    assert_eq!(row.id.as_ref(), "asset-target");
                    assert_eq!(row.version_size.as_str(), "original");
                    let path = row.local_path.as_ref().unwrap();
                    assert!(
                        path.starts_with(media.join("ordinary")),
                        "ordinary pass owns output: {path:?}"
                    );
                    assert_eq!(std::fs::read(path).unwrap(), bytes);
                    assert!(matches!(db.legacy_child_provenance("SharedSync-test", "asset-target", "original").await.unwrap(),
                        Some(crate::state::db::trusted_legacy::LegacyReceiptProvenance::VerifiedSource)));
                } else {
                    assert!(
                        targets.is_empty(),
                        "refused target must not gain a downloaded receipt"
                    );
                }
                assert!(
                    !media.join("hidden").exists(),
                    "sparse recovery must not write the Hidden path"
                );
            }
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
                    usize::from(can_recover && cycle == 0),
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
                    if case == Companion::HiddenTarget {
                        assert!(
                            observed_calls <= 2,
                            "hidden target can only receive one bounded pair confirmation"
                        );
                    } else {
                        assert_eq!(
                            observed_calls,
                            if selected && owner_matches && plan_eligible {
                                2
                            } else {
                                0
                            },
                            "unselected or wrong scope must not be requested"
                        );
                    }
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
