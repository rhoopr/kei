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
        assert_eq!(
            result.stats.downloaded,
            if cycle == 1 && transition == Transition::MixedPathChange {
                2
            } else {
                usize::from(cycle < 2)
            },
            "{selection:?} cycle {cycle}: {result:?}"
        );
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
            "selected smart observation must carry proof"
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
