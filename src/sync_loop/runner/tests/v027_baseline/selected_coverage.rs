//! Actual cycle selection coverage after sparse provider changes.
use super::{BOUNDARY, MEDIA, OLD_TOKEN, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone, Copy, Debug)]
enum Selection {
    Hidden,
    Favorites,
    Recent,
}

#[derive(Clone)]
struct Session {
    cycle: usize,
    selection: Selection,
    incomplete_window: bool,
    original: Arc<Vec<Value>>,
    next: Arc<Vec<Value>>,
    queries: Arc<AtomicUsize>,
}
impl Session {
    fn current(&self) -> Vec<Value> {
        if self.incomplete_window {
            return self.original.as_ref().clone();
        }
        match self.selection {
            Selection::Recent if self.cycle > 0 => self.next.as_ref().clone(),
            Selection::Recent => self
                .original
                .iter()
                .chain(self.next.iter())
                .cloned()
                .collect(),
            _ if self.cycle > 0 => self
                .original
                .iter()
                .chain(self.next.iter())
                .cloned()
                .collect(),
            _ => self.original.as_ref().clone(),
        }
    }
}
#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        assert!(!body.contains(OLD_TOKEN));
        let request: Value = serde_json::from_str(&body)?;
        if url.ends_with("/zones/list") {
            return Ok(json!({"zones":[{"zoneID":zone(),"syncToken":BOUNDARY,"deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response((self.current().len() / 2) as u64));
        }
        if url.contains("/records/query?") {
            self.queries.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0) as usize;
            let limit = request["resultsLimit"].as_u64().unwrap_or(200) as usize;
            let records: Vec<_> = self
                .current()
                .chunks(2)
                .skip(offset)
                .take(limit)
                .flat_map(|pair| pair.iter().cloned())
                .collect();
            if self.incomplete_window {
                return Ok(json!({"records":records}));
            }
            return Ok(json!({"records":records,"syncToken":"query-only-selection-proof"}));
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone());
            let records = if self.cycle == 1 {
                match self.selection {
                    Selection::Recent => vec![
                        json!({"recordName":"asset-selected-original","recordType":"CPLAsset","deleted":true}),
                    ],
                    Selection::Hidden => vec![
                        json!({"recordName":"asset-selected-next","recordType":"CPLAsset",
                        "fields":{"isHidden":{"value":1,"type":"INT64"}}}),
                    ],
                    Selection::Favorites => vec![
                        json!({"recordName":"asset-selected-next","recordType":"CPLAsset",
                        "fields":{"isFavorite":{"value":1,"type":"INT64"}}}),
                    ],
                }
            } else {
                Vec::new()
            };
            return Ok(json!({"zones":[{"zoneID":zone(),"records":records,
                "syncToken":format!("selected-after-{}",self.cycle+1),"moreComing":false}]}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .original
                .iter()
                .chain(self.next.iter())
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        anyhow::bail!("unexpected selected-coverage fixture endpoint: {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_selected_hidden_sparse_delta_downloads_current_member_then_stays_quiet() {
    selected_contract(Selection::Hidden, false, Transition::Stable).await;
}
#[tokio::test]
async fn v027_selected_favorites_sparse_delta_downloads_current_member_then_stays_quiet() {
    selected_contract(Selection::Favorites, false, Transition::Stable).await;
}
#[tokio::test]
async fn v027_recent_removal_selects_unchanged_next_member_then_stays_quiet() {
    selected_contract(Selection::Recent, false, Transition::Stable).await;
}

#[tokio::test]
async fn v027_recent_short_window_without_token_cannot_authorize_baseline_bridge() {
    selected_contract(Selection::Recent, true, Transition::Stable).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transition {
    Stable,
    PathChange,
    MixedPathChange,
    #[cfg(feature = "xmp")]
    SidecarPathChange,
}

#[tokio::test]
async fn v027_hidden_template_change_preserves_recorded_paths_then_stays_quiet() {
    selected_contract(Selection::Hidden, false, Transition::PathChange).await;
}

#[tokio::test]
async fn v027_favorites_mixed_template_change_preserves_paths_and_current_membership() {
    selected_contract(Selection::Favorites, false, Transition::MixedPathChange).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn v027_smart_template_change_preserves_sidecar_bytes_and_timestamps() {
    selected_contract(Selection::Hidden, false, Transition::SidecarPathChange).await;
}

async fn selected_contract(selection: Selection, incomplete_window: bool, transition: Transition) {
    let server = crate::start_wiremock_or_skip!();
    for name in ["original", "next"] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(
                if name == "next" && transition == Transition::MixedPathChange {
                    2 // Two newly selected physical destinations, not a copy of old media.
                } else {
                    u64::from(name == "original" || !incomplete_window)
                },
            )
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let token_key = sync_token_key(ZONE);
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
    let flag = match selection {
        Selection::Hidden => Some("isHidden"),
        Selection::Favorites => Some("isFavorite"),
        Selection::Recent => None,
    };
    if let Some(flag) = flag {
        for records in [&mut original, &mut next] {
            records[1]["fields"][flag] = json!({"value":1,"type":"INT64"});
        }
    }
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut original_mtime = None;
    #[cfg(feature = "xmp")]
    let mut original_sidecar: Option<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)> = None;
    #[cfg(not(feature = "xmp"))]
    let original_sidecar: Option<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)> = None;
    for cycle in 0..if incomplete_window { 1 } else { 3 } {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 0 {
            db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
                .await
                .unwrap();
        }
        let session = Session {
            cycle,
            selection,
            incomplete_window,
            original: Arc::new(original.clone()),
            next: Arc::new(next.clone()),
            queries: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
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
        let (kind, album) = match selection {
            Selection::Recent => (PassKind::Unfiled, library.all()),
            Selection::Hidden => (
                PassKind::SmartFolder,
                library.albums().await.unwrap().remove("Hidden").unwrap(),
            ),
            Selection::Favorites => (
                PassKind::SmartFolder,
                library.albums().await.unwrap().remove("Favorites").unwrap(),
            ),
        };
        let mut selected = make_run_cycle_library_state_with_passes(
            ZONE,
            &token_key,
            vec![AlbumPass {
                kind,
                album,
                exclude_ids: Arc::default(),
            }],
        );
        if transition == Transition::MixedPathChange {
            selected.plan.passes.push(AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::default(),
            });
            selected.pass_scope.include_unfiled = true;
        }
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = if transition == Transition::MixedPathChange {
                "selected"
            } else {
                "none"
            }
            .into();
            config.folder_structure_smart_folders =
                if transition != Transition::Stable && cycle > 0 {
                    "new-selected"
                } else {
                    "selected"
                }
                .into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = transition == Transition::SidecarPathChange;
            }
            if matches!(selection, Selection::Recent) {
                config.recent = Some(if incomplete_window { 2 } else { 1 });
            }
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
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        if !incomplete_window {
            assert_eq!(
                result.failed_count, 0,
                "{selection:?} cycle {cycle}: {result:?}"
            );
        }
        if incomplete_window {
            assert!(
                db.get_metadata(&token_key).await.unwrap().is_none(),
                "a short query without EOF or a reached cap must retain the boundary: {result:?}"
            );
            assert_eq!(std::fs::read(root.join("original.JPG")).unwrap(), MEDIA);
            continue;
        }
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap(),
            Some(format!("selected-after-{}", cycle + 1)),
            "{selection:?} cycle {cycle}: {result:?}"
        );
        assert!(
            db.get_metadata(&format!("observed_selection:v27:{ZONE}"))
                .await
                .unwrap()
                .is_some(),
            "selected smart observation must carry proof: {selection:?} cycle {cycle}, result={result:?}"
        );
        let directory = if matches!(selection, Selection::Recent) {
            root.clone()
        } else {
            root.join("selected")
        };
        let first = directory.join("original.JPG");
        if cycle == 0 {
            assert_eq!(std::fs::read(&first).unwrap(), MEDIA);
            original_mtime = Some(std::fs::metadata(&first).unwrap().modified().unwrap());
            #[cfg(feature = "xmp")]
            if transition == Transition::SidecarPathChange {
                let path = first.with_file_name("original.JPG.xmp");
                original_sidecar = Some((
                    path.clone(),
                    std::fs::read(&path).unwrap(),
                    std::fs::metadata(&path).unwrap().modified().unwrap(),
                ));
            }
            assert!(!directory.join("next.JPG").exists());
        } else {
            let next_directory = if transition == Transition::Stable {
                directory.clone()
            } else {
                root.join("new-selected")
            };
            assert_eq!(
                std::fs::read(next_directory.join("next.JPG")).unwrap(),
                MEDIA
            );
            if transition == Transition::MixedPathChange {
                assert_eq!(
                    std::fs::read(directory.join("next.JPG")).unwrap(),
                    MEDIA,
                    "the independent Unfiled destination must exist on both later cycles"
                );
            }
            if transition != Transition::Stable {
                assert!(
                    !root.join("new-selected/original.JPG").exists(),
                    "template change must not create another copy of recorded content"
                );
            }
            if let Some((path, bytes, mtime)) = &original_sidecar {
                assert_eq!(&std::fs::read(path).unwrap(), bytes);
                assert_eq!(&std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
            }
            assert_eq!(std::fs::read(&first).unwrap(), MEDIA);
            assert_eq!(
                Some(std::fs::metadata(&first).unwrap().modified().unwrap()),
                original_mtime
            );
        }
        assert_eq!(
            result.stats.downloaded,
            if cycle == 1 && transition == Transition::MixedPathChange {
                2
            } else {
                usize::from(cycle < 2)
            },
            "{selection:?} cycle {cycle}: {result:?}"
        );
        if cycle == 1 {
            assert!(
                session.queries.load(Ordering::SeqCst) > 0,
                "changed selection needs current membership evidence"
            );
        }
        if cycle == 2 {
            assert_eq!(
                session.queries.load(Ordering::SeqCst),
                0,
                "quiet provider must not repeat inventory"
            );
        }
    }
    server.verify().await;
}

mod interruptions;

// Diagnostic: current smart selection must not strand a real old selected writer.
// This test deliberately observes the successor decision; it does not prescribe it.
#[derive(Clone)]
struct OldDebtSmartSession {
    old: Arc<Vec<Value>>,
    favorite: Arc<Vec<Value>>,
    smart: Arc<std::sync::atomic::AtomicBool>,
    smart_queries: Arc<AtomicUsize>,
    old_scoped_lookups: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for OldDebtSmartSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        eprintln!("fixture_request url={url} body={request}");
        if url.ends_with("/zones/list") {
            return Ok(json!({"zones":[{"zoneID":zone(),"syncToken":BOUNDARY,"deleted":false}]}));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(1));
        }
        if url.contains("/records/query?") {
            let filters = request["query"]["filterBy"].as_array().unwrap();
            let selected_smart = self.smart.load(Ordering::SeqCst);
            if selected_smart {
                assert!(
                    filters.iter().any(|f| f["fieldName"] == "smartAlbum"
                        && f["fieldValue"]["value"] == "FAVORITE"),
                    "expected genuine Favorites query: {request}"
                );
                self.smart_queries.fetch_add(1, Ordering::SeqCst);
            }
            let offset = filters
                .iter()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                if selected_smart {
                    self.favorite.as_ref().clone()
                } else {
                    self.old.as_ref().clone()
                }
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"smart-query-observation"}));
        }
        if url.contains("/changes/zone?") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"records":[],"syncToken":"smart-successor","moreComing":false}]}),
            );
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            if url.contains("/private/records/lookup?")
                && request["zoneID"] == zone()
                && requested
                    .iter()
                    .any(|record| record["recordName"] == "asset-old-obligation")
            {
                self.old_scoped_lookups.fetch_add(1, Ordering::SeqCst);
            }
            let records: Vec<_> = self
                .old
                .iter()
                .chain(self.favorite.iter())
                .filter(|record| {
                    requested
                        .iter()
                        .any(|r| r["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        anyhow::bail!("unexpected old-debt smart fixture endpoint: {url}")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn format1_unfinished_work_survives_smart_selection_and_quiet_reopen() {
    format1_old_selection_scenario(false).await;
}

#[tokio::test]
async fn format1_unfinished_work_survives_unfiled_toggle_with_production_enum_hash() {
    format1_old_selection_scenario(true).await;
}

async fn format1_old_selection_scenario(production_enum: bool) {
    use crate::icloud::photos::inbox::ShadowCapture;
    use crate::sync_cycle::sync_token_key;
    use std::sync::atomic::AtomicBool;
    let server = crate::start_wiremock_or_skip!();
    let old_ready = Arc::new(AtomicBool::new(false));
    let old_requests = Arc::new(AtomicUsize::new(0));
    let ready = old_ready.clone();
    let requests = old_requests.clone();
    Mock::given(method("GET"))
        .and(path("/old"))
        .respond_with(move |_: &wiremock::Request| {
            requests.fetch_add(1, Ordering::SeqCst);
            if ready.load(Ordering::SeqCst) {
                ResponseTemplate::new(200).set_body_bytes(MEDIA)
            } else {
                ResponseTemplate::new(503)
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorite"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap().keep();
    eprintln!("diagnostic_fixture={}", dir.display());
    let root = dir.join("media");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("keep.bin"), b"unrelated-existing-bytes").unwrap();
    let database = dir.join("state.db");
    let token_key = sync_token_key(ZONE);
    let mut old = pair(
        "old-obligation",
        "old.JPG",
        &format!("{}/old", server.uri()),
        21,
    );
    let mut favorite = pair(
        "favorite-new",
        "favorite.JPG",
        &format!("{}/favorite", server.uri()),
        22,
    );
    use sha2::{Digest, Sha256};
    for item in [&mut old, &mut favorite] {
        item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(data_encoding::BASE64.encode(&Sha256::digest(MEDIA)));
    }
    favorite[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
    old[1]["fields"]["isFavorite"] = json!({"value":0,"type":"INT64"});
    let session = OldDebtSmartSession {
        old: Arc::new(old),
        favorite: Arc::new(favorite),
        smart: Arc::new(AtomicBool::new(false)),
        smart_queries: Arc::new(AtomicUsize::new(0)),
        old_scoped_lookups: Arc::new(AtomicUsize::new(0)),
    };
    let db = Arc::new(
        SqliteStateDb::open_owned(&database, &baseline_owner())
            .await
            .unwrap(),
    );
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session.clone()),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
    let old_pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::default(),
    };
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let app_config = make_run_cycle_config();
    let legacy_hashes: [Arc<str>; 2] = [
        download::compute_config_hash_for_unfiled(&app_config, false).into(),
        download::compute_config_hash_for_unfiled(&app_config, true).into(),
    ];
    let mut old_config = builder(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        ZONE.into(),
    );
    if production_enum {
        let config = Arc::make_mut(&mut old_config);
        config.enum_config_hash = Some(legacy_hashes[1].clone());
        config.legacy_enum_hashes = Some(legacy_hashes.clone());
    }
    let old_passes = [old_pass];
    let old_result = download::TEST_GENERATION_FORMAT
        .scope(
            1,
            download::download_photos_with_sync(
                &reqwest::Client::new(),
                &old_passes,
                old_config,
                download::DownloadControls::download_hidden(),
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(old_result.stats.downloaded, 0, "{old_result:?}");
    assert!(
        old_requests.load(Ordering::SeqCst) > 0,
        "the old writer must really have attempted the transfer"
    );
    let (old_generation, old_path, old_checksum, old_pass_key, old_scope): (String, String, String, String, String) = db.acquire_lock("old admitted destination oracle").unwrap().query_row(
        "SELECT g.id,w.compat_path,w.checksum,w.pass_key,g.scope FROM provider_active_generations g JOIN provider_active_destinations w ON w.generation=g.id WHERE json_extract(CAST(g.specification AS TEXT),'$.format')=1 AND w.child='asset-old-obligation' AND w.admitted=1 AND w.verified_media=0", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert!(!std::path::Path::new(&old_path).exists());
    let before_receipts: i64 = db
        .acquire_lock("old receipt before transition")
        .unwrap()
        .query_row(
            "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before_receipts, 0);
    db.make_selection_retry_due(baseline_owner(), "asset-old-obligation".into())
        .await
        .unwrap();
    drop(old_passes);
    drop(builder);
    drop(library);
    drop(db);
    session.smart.store(true, Ordering::SeqCst);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut old_attempts_before_quiet = 0;
    let mut actual_successors = Vec::new();
    let mut reporting_known = false;
    let mut completed_gets = None;
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
        let favorite = library.albums().await.unwrap().remove("Favorites").unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(
            ZONE,
            &token_key,
            vec![AlbumPass {
                kind: PassKind::SmartFolder,
                album: favorite,
                exclude_ids: Arc::default(),
            }],
        );
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            if production_enum {
                config.enum_config_hash = Some(legacy_hashes[0].clone());
                config.legacy_enum_hashes = Some(legacy_hashes.clone());
            }
            Arc::new(config)
        };
        let prior_token = db.get_metadata(&token_key).await.unwrap();
        let old_gate: (String, String, String, i64, i64, i64) = db.acquire_lock("diagnostic old gate").unwrap().query_row(
            "SELECT g.config_hash,d.pass_key,d.admission,d.next_retry_at,w.admitted,w.verified_media FROM provider_active_generations g JOIN provider_active_decisions d ON d.generation=g.id JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child WHERE g.id=?1 AND d.child='asset-old-obligation'", [&old_generation],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        eprintln!("cycle_start={cycle} token={prior_token:?} old_gate={old_gate:?}");
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let successor = db.get_metadata(&token_key).await.unwrap();
        actual_successors.push(successor.clone());
        eprintln!("cycle={cycle} successor={successor:?} result={result:?}");
        if cycle == 0 {
            assert!(
                session.smart_queries.load(Ordering::SeqCst) > 0,
                "Favorites membership requires the real selected query"
            );
            assert_eq!(
                result.stats.downloaded, 1,
                "healthy selected work should progress: {result:?}"
            );
            let (favorite_path, favorite_verified): (String, i64) = db.acquire_lock("new selected destination").unwrap().query_row(
                "SELECT w.compat_path,w.verified_media FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE json_extract(CAST(g.specification AS TEXT),'$.format')=2 AND w.child='asset-favorite-new'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(favorite_verified, 1);
            assert_eq!(std::fs::read(favorite_path).unwrap(), MEDIA);
            assert_eq!(db.acquire_lock("unselected old child is not newly admitted").unwrap().query_row::<i64,_,_>(
                "SELECT count(*) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE json_extract(CAST(g.specification AS TEXT),'$.format')=2 AND d.child='asset-old-obligation' AND d.admission='admitted'", [], |r| r.get(0)).unwrap(), 0);
            old_attempts_before_quiet = old_requests.load(Ordering::SeqCst);
        }
        let (path, checksum, admitted, verified): (String, String, i64, i64) = db.acquire_lock("old destination still owned").unwrap().query_row(
            "SELECT compat_path,checksum,admitted,verified_media FROM provider_active_destinations WHERE generation=?1 AND child='asset-old-obligation'", [&old_generation],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(
            (path, checksum, admitted),
            (old_path.clone(), old_checksum.clone(), 1)
        );
        let report = db.recorded_selection_work().await.unwrap();
        assert!(report.historical_roots > 0);
        let summary = db.get_summary().await.unwrap();
        eprintln!(
            "cycle_status={cycle} historical_roots={} pending={} failed={}",
            report.historical_roots, summary.pending, summary.failed
        );
        reporting_known |= summary.pending > 0 || summary.failed > 0;
        let observations = db.observed_selection_status().await.unwrap();
        reporting_known |= observations
            .iter()
            .any(|o| o.settlement_unknown || o.pending_media || o.unresolved);
        if verified == 0 {
            assert!(!std::path::Path::new(&old_path).exists());
            assert_eq!(
                db.acquire_lock("old receipt remains absent")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                before_receipts
            );
        } else {
            assert_eq!(std::fs::read(&old_path).unwrap(), MEDIA);
            assert!(
                db.acquire_lock("old verified receipt")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap()
                    > 0
            );
        }
        assert_eq!(
            std::fs::read(root.join("keep.bin")).unwrap(),
            b"unrelated-existing-bytes"
        );
        if cycle == 0 {
            assert_eq!(
                verified, 0,
                "old work must remain unfinished through the transition"
            );
            db.make_selection_retry_due(baseline_owner(), "asset-old-obligation".into())
                .await
                .unwrap();
            old_ready.store(true, Ordering::SeqCst);
            let response = reqwest::get(format!("{}/old", server.uri())).await.unwrap();
            assert!(response.status().is_success());
            assert_eq!(response.bytes().await.unwrap().as_ref(), MEDIA);
            old_attempts_before_quiet = old_requests.load(Ordering::SeqCst);
        } else if cycle == 1 {
            assert_eq!(verified, 1, "old work must finish on the first quiet cycle");
            completed_gets = Some(old_requests.load(Ordering::SeqCst));
        } else {
            assert_eq!(verified, 1);
            assert_eq!(
                Some(old_requests.load(Ordering::SeqCst)),
                completed_gets,
                "quiet cycle repeated the old transfer"
            );
        }
    }
    let db = SqliteStateDb::open_owned(&database, &baseline_owner())
        .await
        .unwrap();
    let verified: i64 = db.acquire_lock("final old work").unwrap().query_row(
        "SELECT verified_media FROM provider_active_destinations WHERE generation=?1 AND child='asset-old-obligation'", [&old_generation], |r| r.get(0)).unwrap();
    assert!(
        reporting_known,
        "unfinished old work was not reported as unresolved during transition: successors={actual_successors:?}"
    );
    assert_eq!(
        verified,
        1,
        "old work did not complete in the quiet cycle: successors={actual_successors:?}, old_gets={} before_quiet={old_attempts_before_quiet}",
        old_requests.load(Ordering::SeqCst)
    );
    assert!(
        old_requests.load(Ordering::SeqCst) > old_attempts_before_quiet,
        "completion must use the enabled old transfer"
    );
    assert_eq!(std::fs::read(&old_path).unwrap(), MEDIA);
    let (scope, pass, master, checksum, size, native_path, source, local): (String, String, String, String, i64, String, String, String) = db.acquire_lock("exact reopened old receipt").unwrap().query_row(
        "SELECT scope,origin_pass,master,provider_checksum,provider_size,native_path,source_checksum,local_checksum FROM download_receipts WHERE child='asset-old-obligation'", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).unwrap();
    assert_eq!(
        (scope, pass, master, checksum, size),
        (
            old_scope,
            old_pass_key,
            "old-obligation".into(),
            old_checksum,
            MEDIA.len() as i64
        )
    );
    assert_eq!(
        serde_json::from_str::<Value>(&native_path).unwrap(),
        json!({"encoding":"utf8","path":old_path})
    );
    let expected_digest = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
    assert_eq!(source, expected_digest);
    assert_eq!(local, expected_digest);
    assert!(
        db.acquire_lock("reopened old completion receipt")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
                [],
                |r| r.get(0)
            )
            .unwrap()
            > 0
    );
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum FrozenCase {
    Absent,
    Regular,
    LeafLink,
    ParentLink,
}

#[cfg(unix)]
#[tokio::test]
async fn composed_frozen_retry_absent() {
    composed_frozen_retry(FrozenCase::Absent).await;
}
#[cfg(unix)]
#[tokio::test]
async fn composed_frozen_retry_regular() {
    composed_frozen_retry(FrozenCase::Regular).await;
}
#[cfg(unix)]
#[tokio::test]
async fn composed_frozen_retry_leaf_symlink() {
    composed_frozen_retry(FrozenCase::LeafLink).await;
}
#[cfg(unix)]
#[tokio::test]
async fn composed_frozen_retry_parent_symlink() {
    composed_frozen_retry(FrozenCase::ParentLink).await;
}

#[cfg(unix)]
async fn composed_frozen_retry(case: FrozenCase) {
    let production_enum = true;
    fn tree_entries(path: &std::path::Path, entries: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            entries.push(path.clone());
            if metadata.is_dir() {
                tree_entries(&path, entries);
            }
        }
    }
    use crate::icloud::photos::inbox::ShadowCapture;
    use crate::sync_cycle::sync_token_key;
    use std::sync::atomic::AtomicBool;
    let server = crate::start_wiremock_or_skip!();
    let old_ready = Arc::new(AtomicBool::new(false));
    let old_requests = Arc::new(AtomicUsize::new(0));
    let ready = old_ready.clone();
    let requests = old_requests.clone();
    Mock::given(method("GET"))
        .and(path("/old"))
        .respond_with(move |_: &wiremock::Request| {
            requests.fetch_add(1, Ordering::SeqCst);
            if ready.load(Ordering::SeqCst) {
                ResponseTemplate::new(200).set_body_bytes(MEDIA)
            } else {
                ResponseTemplate::new(503)
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/favorite"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let occupant_bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/metadata.jpg"
    ));
    Mock::given(method("GET"))
        .and(path("/occupant"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(occupant_bytes.as_slice()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap().keep();
    eprintln!("diagnostic_fixture={}", dir.display());
    let root = dir.join("media");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("keep.bin"), b"unrelated-existing-bytes").unwrap();
    let database = dir.join("state.db");
    let token_key = sync_token_key(ZONE);
    let mut old = pair(
        "old-obligation",
        "old.JPG",
        &format!("{}/old", server.uri()),
        21,
    );
    let mut favorite = pair(
        "favorite-new",
        "favorite.JPG",
        &format!("{}/favorite", server.uri()),
        22,
    );
    use sha2::{Digest, Sha256};
    for item in [&mut old, &mut favorite] {
        item[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(data_encoding::BASE64.encode(&Sha256::digest(MEDIA)));
    }
    favorite[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
    old[1]["fields"]["isFavorite"] = json!({"value":0,"type":"INT64"});
    let mut occupant = pair(
        "nominal-occupant",
        "old.JPG",
        &format!("{}/occupant", server.uri()),
        23,
    );
    occupant[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(occupant_bytes.len());
    occupant[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(data_encoding::BASE64.encode(&Sha256::digest(occupant_bytes)));
    occupant.extend(old);
    let session = OldDebtSmartSession {
        old: Arc::new(occupant),
        favorite: Arc::new(favorite),
        smart: Arc::new(AtomicBool::new(false)),
        smart_queries: Arc::new(AtomicUsize::new(0)),
        old_scoped_lookups: Arc::new(AtomicUsize::new(0)),
    };
    let db = Arc::new(
        SqliteStateDb::open_owned(&database, &baseline_owner())
            .await
            .unwrap(),
    );
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(session.clone()),
        Arc::new(zone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
    let old_pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::default(),
    };
    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let app_config = make_run_cycle_config();
    let legacy_hashes: [Arc<str>; 2] = [
        download::compute_config_hash_for_unfiled(&app_config, false).into(),
        download::compute_config_hash_for_unfiled(&app_config, true).into(),
    ];
    let mut old_config = builder(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        ZONE.into(),
    );
    if production_enum {
        let config = Arc::make_mut(&mut old_config);
        config.folder_structure = "frozen".into();
        config.folder_structure_smart_folders = "selected".into();
        config.enum_config_hash = Some(legacy_hashes[1].clone());
        config.legacy_enum_hashes = Some(legacy_hashes.clone());
    }
    let old_passes = [old_pass];
    let old_result = download::TEST_GENERATION_FORMAT
        .scope(
            1,
            download::download_photos_with_sync(
                &reqwest::Client::new(),
                &old_passes,
                old_config.clone(),
                download::DownloadControls::download_hidden(),
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(old_result.stats.downloaded, 1, "{old_result:?}");
    if old_requests.load(Ordering::SeqCst) == 0 {
        // The first planner can observe the occupant before its receipt is committed.
        // Retry the real admitted old obligation once that publication has completed.
        db.make_selection_retry_due(baseline_owner(), "asset-old-obligation".into())
            .await
            .unwrap();
        download::TEST_GENERATION_FORMAT
            .scope(
                1,
                download::download_photos_with_sync(
                    &reqwest::Client::new(),
                    &old_passes,
                    old_config,
                    download::DownloadControls::download_hidden(),
                    CancellationToken::new(),
                ),
            )
            .await
            .unwrap();
    }
    assert!(
        old_requests.load(Ordering::SeqCst) > 0,
        "the old writer must really have attempted the transfer"
    );
    let (old_generation, old_path, old_checksum, old_pass_key, old_scope): (String, String, String, String, String) = db.acquire_lock("old admitted destination oracle").unwrap().query_row(
        "SELECT g.id,w.compat_path,w.checksum,w.pass_key,g.scope FROM provider_active_generations g JOIN provider_active_destinations w ON w.generation=g.id WHERE json_extract(CAST(g.specification AS TEXT),'$.format')=1 AND w.child='asset-old-obligation' AND w.admitted=1 AND w.verified_media=0", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
    assert!(!std::path::Path::new(&old_path).exists());
    let nominal = root.join("frozen/old.JPG");
    assert_ne!(std::path::Path::new(&old_path), nominal);
    assert_eq!(std::fs::read(&nominal).unwrap(), occupant_bytes);
    let nominal_mtime = std::fs::metadata(&nominal).unwrap().modified().unwrap();
    let nominal_receipt: (String, String, String, String) = db.acquire_lock("nominal receipt").unwrap().query_row(
        "SELECT scope,origin_pass,master,native_path FROM download_receipts WHERE child='asset-nominal-occupant'", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(nominal_receipt.0, old_scope);
    assert_eq!(nominal_receipt.1, old_pass_key);
    assert_eq!(nominal_receipt.2, "nominal-occupant");
    assert_eq!(
        serde_json::from_str::<Value>(&nominal_receipt.3).unwrap(),
        json!({"encoding":"utf8","path":nominal.to_str().unwrap()})
    );
    let before_receipts: i64 = db
        .acquire_lock("old receipt before transition")
        .unwrap()
        .query_row(
            "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before_receipts, 0);
    db.make_selection_retry_due(baseline_owner(), "asset-old-obligation".into())
        .await
        .unwrap();
    drop(old_passes);
    drop(builder);
    drop(library);
    drop(db);
    session.smart.store(true, Ordering::SeqCst);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut old_attempts_before_quiet = 0;
    let mut actual_successors = Vec::new();
    let mut reporting_known = false;
    let mut completed_gets = None;
    let mut initial_entries = None;
    let mut obstruction_mtime = None;
    let old_file = std::path::PathBuf::from(&old_path);
    let outside = dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    let referent = outside.join("referent");
    std::fs::write(&referent, b"outside sentinel").unwrap();
    match case {
        FrozenCase::Absent => {}
        FrozenCase::Regular => std::fs::write(&old_file, b"occupied sentinel").unwrap(),
        FrozenCase::LeafLink => std::os::unix::fs::symlink(&referent, &old_file).unwrap(),
        FrozenCase::ParentLink => {
            std::fs::rename(root.join("frozen"), outside.join("moved")).unwrap();
            std::os::unix::fs::symlink(outside.join("moved"), root.join("frozen")).unwrap();
        }
    }
    let blocked_gets = old_requests.load(Ordering::SeqCst);
    let lookups_before_transition = session.old_scoped_lookups.load(Ordering::SeqCst);
    if !matches!(case, FrozenCase::Absent) {
        old_ready.store(true, Ordering::SeqCst);
    }
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(session.clone()),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
        let favorite = library.albums().await.unwrap().remove("Favorites").unwrap();
        let mut selected = make_run_cycle_library_state_with_passes(
            ZONE,
            &token_key,
            vec![AlbumPass {
                kind: PassKind::SmartFolder,
                album: favorite,
                exclude_ids: Arc::default(),
            }],
        );
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            if production_enum {
                config.folder_structure = "frozen".into();
                config.folder_structure_smart_folders = "selected".into();
                config.enum_config_hash = Some(legacy_hashes[0].clone());
                config.legacy_enum_hashes = Some(legacy_hashes.clone());
            }
            Arc::new(config)
        };
        let prior_token = db.get_metadata(&token_key).await.unwrap();
        let old_gate: (String, String, String, i64, i64, i64) = db.acquire_lock("diagnostic old gate").unwrap().query_row(
            "SELECT g.config_hash,d.pass_key,d.admission,d.next_retry_at,w.admitted,w.verified_media FROM provider_active_generations g JOIN provider_active_decisions d ON d.generation=g.id JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child WHERE g.id=?1 AND d.child='asset-old-obligation'", [&old_generation],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        eprintln!("cycle_start={cycle} token={prior_token:?} old_gate={old_gate:?}");
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let successor = db.get_metadata(&token_key).await.unwrap();
        actual_successors.push(successor.clone());
        eprintln!("cycle={cycle} successor={successor:?} result={result:?}");
        if cycle == 0 {
            assert!(
                session.smart_queries.load(Ordering::SeqCst) > 0,
                "Favorites membership requires the real selected query"
            );
            assert_eq!(
                result.stats.downloaded, 1,
                "healthy selected work should progress: {result:?}"
            );
            let (favorite_path, favorite_verified): (String, i64) = db.acquire_lock("new selected destination").unwrap().query_row(
                "SELECT w.compat_path,w.verified_media FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE json_extract(CAST(g.specification AS TEXT),'$.format')=2 AND w.child='asset-favorite-new'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(favorite_verified, 1);
            assert_eq!(std::fs::read(favorite_path).unwrap(), MEDIA);
            assert_eq!(db.acquire_lock("unselected old child is not newly admitted").unwrap().query_row::<i64,_,_>(
                "SELECT count(*) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE json_extract(CAST(g.specification AS TEXT),'$.format')=2 AND d.child='asset-old-obligation' AND d.admission='admitted'", [], |r| r.get(0)).unwrap(), 0);
            old_attempts_before_quiet = old_requests.load(Ordering::SeqCst);
        }
        let (path, checksum, admitted, verified): (String, String, i64, i64) = db.acquire_lock("old destination still owned").unwrap().query_row(
            "SELECT compat_path,checksum,admitted,verified_media FROM provider_active_destinations WHERE generation=?1 AND child='asset-old-obligation'", [&old_generation],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(
            (path, checksum, admitted),
            (old_path.clone(), old_checksum.clone(), 1)
        );
        let report = db.recorded_selection_work().await.unwrap();
        assert!(report.historical_roots > 0);
        let summary = db.get_summary().await.unwrap();
        eprintln!(
            "cycle_status={cycle} historical_roots={} pending={} failed={}",
            report.historical_roots, summary.pending, summary.failed
        );
        reporting_known |= summary.pending > 0 || summary.failed > 0;
        let observations = db.observed_selection_status().await.unwrap();
        reporting_known |= observations
            .iter()
            .any(|o| o.settlement_unknown || o.pending_media || o.unresolved);
        if verified == 0 {
            if matches!(case, FrozenCase::Absent) {
                assert!(!old_file.exists());
            } else {
                match case {
                    FrozenCase::Regular => {
                        assert_eq!(std::fs::read(&old_file).unwrap(), b"occupied sentinel")
                    }
                    FrozenCase::LeafLink => {
                        assert_eq!(std::fs::read_link(&old_file).unwrap(), referent);
                        assert_eq!(std::fs::read(&referent).unwrap(), b"outside sentinel");
                    }
                    FrozenCase::ParentLink => {
                        assert_eq!(
                            std::fs::read_link(root.join("frozen")).unwrap(),
                            outside.join("moved")
                        );
                        assert!(
                            !outside
                                .join("moved")
                                .join(old_file.file_name().unwrap())
                                .exists()
                        );
                    }
                    FrozenCase::Absent => assert!(
                        !matches!(case, FrozenCase::Absent),
                        "absent case entered obstruction branch"
                    ),
                }
            }
            assert_eq!(
                db.acquire_lock("old receipt remains absent")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                before_receipts
            );
        } else {
            assert_eq!(std::fs::read(&old_path).unwrap(), MEDIA);
            assert!(
                db.acquire_lock("old verified receipt")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap()
                    > 0
            );
        }
        assert_eq!(
            std::fs::read(root.join("keep.bin")).unwrap(),
            b"unrelated-existing-bytes"
        );
        if cycle == 0 {
            assert_eq!(
                verified, 0,
                "old work must remain unfinished through the transition"
            );
            db.make_selection_retry_due(baseline_owner(), "asset-old-obligation".into())
                .await
                .unwrap();
            if matches!(case, FrozenCase::Absent) {
                old_ready.store(true, Ordering::SeqCst);
            } else {
                assert_eq!(
                    old_requests.load(Ordering::SeqCst),
                    blocked_gets,
                    "blocked replay must not transfer while healthy current work progresses"
                );
                assert!(
                    session.old_scoped_lookups.load(Ordering::SeqCst) > lookups_before_transition,
                    "the original scoped child lookup must execute after selection changes"
                );
                let diagnostic: String = db.acquire_lock("frozen path refusal diagnostic").unwrap().query_row(
                    "SELECT last_error FROM assets WHERE id='asset-old-obligation' AND version_size='original'", [], |r| r.get(0)).unwrap();
                let diagnostic: Value =
                    serde_json::from_str(diagnostic.strip_prefix("existing_match_v1:").unwrap())
                        .unwrap();
                let attempts = diagnostic["attempts"].as_array().unwrap();
                let reason = if matches!(case, FrozenCase::Regular) {
                    "recorded_destination_occupied_without_receipt"
                } else {
                    "candidate_unreadable_unsafe_or_changed"
                };
                assert!(
                    attempts.iter().any(|attempt| {
                        attempt["generation"] == old_generation
                            && attempt["pass"] == old_pass_key
                            && attempt["child"] == "asset-old-obligation"
                            && attempt["destination"] == json!({"encoding":"utf8","path":old_path})
                            && attempt["checksum"] == old_checksum
                            && attempt["explanation"]["candidates"].as_array().is_some_and(
                                |candidates| {
                                    candidates.iter().any(|candidate| {
                                        candidate["path"]
                                            == json!({"encoding":"utf8","path":old_path})
                                            && candidate["reason"] == reason
                                    })
                                },
                            )
                    }),
                    "exact old frozen destination refusal must be durable: {diagnostic}"
                );
            }
            old_attempts_before_quiet = old_requests.load(Ordering::SeqCst);
            if !matches!(case, FrozenCase::Absent) {
                let obstruction = if matches!(case, FrozenCase::ParentLink) {
                    root.join("frozen")
                } else {
                    old_file.clone()
                };
                obstruction_mtime = Some(
                    std::fs::symlink_metadata(obstruction)
                        .unwrap()
                        .modified()
                        .unwrap(),
                );
            }
            let mut entries = Vec::new();
            tree_entries(&root, &mut entries);
            tree_entries(&outside, &mut entries);
            entries.sort();
            initial_entries = Some(entries);
        } else if matches!(case, FrozenCase::Absent) {
            assert_eq!(verified, 1, "old work must finish on the first quiet cycle");
            if cycle == 1 {
                completed_gets = Some(old_requests.load(Ordering::SeqCst));
            } else {
                assert_eq!(Some(old_requests.load(Ordering::SeqCst)), completed_gets);
            }
        } else {
            assert_eq!(verified, 0, "unsafe frozen destination must remain pending");
            assert_eq!(
                old_requests.load(Ordering::SeqCst),
                old_attempts_before_quiet
            );
        }
        assert_eq!(std::fs::read(&nominal).unwrap(), occupant_bytes);
        assert_eq!(
            std::fs::metadata(&nominal).unwrap().modified().unwrap(),
            nominal_mtime
        );
        let current_receipt: (String, String, String, String) = db.acquire_lock("unchanged nominal receipt").unwrap().query_row(
            "SELECT scope,origin_pass,master,native_path FROM download_receipts WHERE child='asset-nominal-occupant'", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(current_receipt, nominal_receipt);
        if cycle > 0 {
            let mut expected = initial_entries.clone().unwrap();
            if matches!(case, FrozenCase::Absent) {
                expected.push(old_file.clone());
                expected.sort();
            } else {
                let obstruction = if matches!(case, FrozenCase::ParentLink) {
                    root.join("frozen")
                } else {
                    old_file.clone()
                };
                assert_eq!(
                    Some(
                        std::fs::symlink_metadata(obstruction)
                            .unwrap()
                            .modified()
                            .unwrap()
                    ),
                    obstruction_mtime
                );
            }
            let mut actual = Vec::new();
            tree_entries(&root, &mut actual);
            tree_entries(&outside, &mut actual);
            actual.sort();
            assert_eq!(actual, expected, "replay must not create an alternate path");
        }
    }
    let db = SqliteStateDb::open_owned(&database, &baseline_owner())
        .await
        .unwrap();
    let verified: i64 = db.acquire_lock("final old work").unwrap().query_row(
        "SELECT verified_media FROM provider_active_destinations WHERE generation=?1 AND child='asset-old-obligation'", [&old_generation], |r| r.get(0)).unwrap();
    assert!(
        reporting_known,
        "unfinished old work was not reported as unresolved during transition: successors={actual_successors:?}"
    );
    if !matches!(case, FrozenCase::Absent) {
        assert_eq!(verified, 0);
        assert_eq!(
            db.acquire_lock("no false old receipt after reopen")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
            0
        );
        assert!(db.recorded_selection_work().await.unwrap().historical_roots > 0);
        return;
    }
    assert_eq!(
        verified,
        1,
        "old work did not complete in the quiet cycle: successors={actual_successors:?}, old_gets={} before_quiet={old_attempts_before_quiet}",
        old_requests.load(Ordering::SeqCst)
    );
    assert!(
        old_requests.load(Ordering::SeqCst) > old_attempts_before_quiet,
        "completion must use the enabled old transfer"
    );
    assert_eq!(std::fs::read(&old_path).unwrap(), MEDIA);
    let asset: (String, String, i64, i64) = db.acquire_lock("reopened old asset identity and dates").unwrap().query_row(
        "SELECT status,local_path,created_at,added_at FROM assets WHERE id='asset-old-obligation' AND version_size='original'", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!(
        asset,
        (
            "downloaded".into(),
            old_path.clone(),
            1_700_000_000,
            1_700_000_000
        )
    );
    let (scope, pass, master, checksum, size, native_path, source, local): (String, String, String, String, i64, String, String, String) = db.acquire_lock("exact reopened old receipt").unwrap().query_row(
        "SELECT scope,origin_pass,master,provider_checksum,provider_size,native_path,source_checksum,local_checksum FROM download_receipts WHERE child='asset-old-obligation'", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).unwrap();
    assert_eq!(
        (scope, pass, master, checksum, size),
        (
            old_scope,
            old_pass_key,
            "old-obligation".into(),
            old_checksum,
            MEDIA.len() as i64
        )
    );
    assert_eq!(
        serde_json::from_str::<Value>(&native_path).unwrap(),
        json!({"encoding":"utf8","path":old_path})
    );
    let expected_digest = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
    assert_eq!(source, expected_digest);
    assert_eq!(local, expected_digest);
    assert!(
        db.acquire_lock("reopened old completion receipt")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM download_receipts WHERE child='asset-old-obligation'",
                [],
                |r| r.get(0)
            )
            .unwrap()
            > 0
    );
}
