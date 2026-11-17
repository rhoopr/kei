use super::{ORIGINAL, assert_bytes, db_count, fixture_owner, records, server, versioned_records};
use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoLibrary, PhotosSession};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use crate::{download, state};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn native_publication_receipts(root: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    conn.prepare("SELECT generation||pass_key||child||version_size||path||local_checksum||source_checksum||progress_hash FROM provider_active_destinations WHERE verified_media=1 ORDER BY generation,pass_key,child,version_size,path").unwrap().query_map([],|row|row.get(0)).unwrap().collect::<Result<_,_>>().unwrap()
}

#[derive(Clone)]
struct Provider {
    records: Vec<Value>,
    members: Vec<&'static str>,
    pages: Vec<Vec<Value>>,
    interrupt: bool,
    cancel: CancellationToken,
}

fn relation(child: &str, container: &str, deleted: bool) -> Value {
    json!({"recordName":format!("{child}-IN-{container}"),"recordType":"CPLContainerRelation","deleted":deleted,"fields":{"containerId":{"value":container},"itemId":{"value":child}}})
}

#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request.pointer("/query/recordType").and_then(Value::as_str)
            == Some("CheckIndexingState")
        {
            return Ok(json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]}));
        }
        let source = versioned_records(&self.records);
        if url.contains("/changes/zone?") {
            let cursor = request["zones"][0]["syncToken"].as_str();
            let index = cursor
                .and_then(|cursor| cursor.strip_prefix("membership-page-"))
                .map_or(0, |index| index.parse::<usize>().unwrap());
            if self.interrupt && index > 0 {
                self.cancel.cancel();
                anyhow::bail!("interrupted membership tail");
            }
            let done = cursor == Some("membership-end");
            let records = if done {
                Vec::new()
            } else if self.pages.is_empty() {
                source.clone()
            } else {
                self.pages[index].clone()
            };
            let more = !done && index + 1 < self.pages.len();
            let token = if more {
                format!("membership-page-{}", index + 1)
            } else {
                "membership-end".to_owned()
            };
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":records,"syncToken":token,"moreComing":more}]}),
            );
        }
        if url.contains("/records/lookup?") {
            let rows: Vec<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|wanted| {
                    source
                        .iter()
                        .find(|record| record["recordName"] == wanted["recordName"])
                        .cloned()
                })
                .collect();
            return Ok(json!({"records":rows}));
        }
        if url.contains("/records/query/batch?") || url.contains("/internal/records/query/batch") {
            return Ok(
                json!({"batch":request["batch"].as_array().unwrap().iter().map(|query| {
                let kind=query["query"]["filterBy"]["fieldValue"]["value"][0].as_str().unwrap();
                let selected=kind.rsplit_once(':').is_none_or(|(_,id)|self.members.contains(&id));
                json!({"records":[{"fields":{"itemCount":{"value":if selected {self.records.len()/2} else {0}}}}]})
            }).collect::<Vec<_>>()}),
            );
        }
        if url.contains("/records/query?") {
            let filters = request["query"]["filterBy"].as_array();
            let offset = filters
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let container = filters
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "parentId")
                .and_then(|filter| filter["fieldValue"]["value"].as_str());
            let selected = container.is_none_or(|container| self.members.contains(&container));
            let mut rows = if offset == 0 && selected {
                source
            } else {
                Vec::new()
            };
            if let Some(container) = container {
                let relations: Vec<_> = rows
                    .iter()
                    .filter(|record| record["recordType"] == "CPLAsset")
                    .map(|record| {
                        relation(record["recordName"].as_str().unwrap(), container, false)
                    })
                    .collect();
                rows.extend(relations);
            }
            return Ok(json!({"records":rows,"syncToken":"membership-query"}));
        }
        anyhow::bail!("unexpected membership fixture route")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn cycle(
    root: &std::path::Path,
    provider: Provider,
    unfiled: bool,
) -> crate::sync_cycle::CycleResult {
    cycle_with_scope(root, provider, unfiled, true).await
}
async fn cycle_with_scope(
    root: &std::path::Path,
    provider: Provider,
    unfiled: bool,
    include_a: bool,
) -> crate::sync_cycle::CycleResult {
    cycle_with_naming(
        root,
        provider,
        unfiled,
        include_a,
        crate::types::EditedNaming::Primary,
    )
    .await
}
async fn cycle_with_naming(
    root: &std::path::Path,
    provider: Provider,
    unfiled: bool,
    include_a: bool,
    naming: crate::types::EditedNaming,
) -> crate::sync_cycle::CycleResult {
    cycle_with_layout(root, provider, unfiled, include_a, naming, "{album}").await
}
async fn cycle_with_layout(
    root: &std::path::Path,
    provider: Provider,
    unfiled: bool,
    include_a: bool,
    naming: crate::types::EditedNaming,
    album_template: &str,
) -> crate::sync_cycle::CycleResult {
    let names: Vec<_> = ["A", "B", "unfiled"]
        .into_iter()
        .filter(|name| (*name != "A" || include_a) && (*name != "unfiled" || unfiled))
        .collect();
    cycle_selected(root, provider, &names, naming, album_template, Vec::new()).await
}

async fn cycle_selected(
    root: &std::path::Path,
    provider: Provider,
    names: &[&str],
    naming: crate::types::EditedNaming,
    album_template: &str,
    bindings: Vec<state::db::provider_generations::BoundAlbumSelection>,
) -> crate::sync_cycle::CycleResult {
    let unfiled = names.contains(&"unfiled");
    let owner = fixture_owner();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let mut passes = Vec::new();
    for &name in names {
        let container = (name != "unfiled").then_some(name);
        let mut album=PhotoAlbum::new(PhotoAlbumConfig {
            params:Arc::default(),service_endpoint:Arc::from("https://example.com"),name:Arc::from(name),
            list_type:Arc::from(if container.is_some() {"CPLContainerRelationLiveByAssetDate"} else {"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"}),
            obj_type:container.map_or_else(||Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),|id|Arc::from(format!("CPLContainerRelationNotDeletedByAssetDate:{id}"))),
            query_filter:container.map(|id|Arc::new(json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":id}}]))),
            page_size:100,zone_id:Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config:crate::retry::RetryConfig::default(),container_id:container.map(Arc::from),cross_zone_sources:Vec::new(),
        },Box::new(provider.clone()));
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
            Arc::from("private"),
        );
        passes.push(AlbumPass {
            kind: if container.is_some() {
                PassKind::Album
            } else {
                PassKind::Unfiled
            },
            album,
            exclude_ids: Arc::default(),
        });
    }
    let mut library =
        make_run_cycle_library_state_with_passes("PrimarySync", "sync_token:PrimarySync", passes);
    library.library = PhotoLibrary::new_stub_with_zone(Box::new(provider.clone()), "PrimarySync");
    if !bindings.is_empty() {
        library.library = authenticated_library(&provider, db.clone()).await;
    }
    library.deleted_album_bindings = bindings;
    let media = root.join("media");
    std::fs::create_dir_all(&media).unwrap();
    let mut config = make_run_cycle_config();
    config.download.directory = media.clone();
    config.download.folder_structure = "unfiled".into();
    config.download.folder_structure_albums = album_template.into();
    config.photos.edited_naming = naming;
    config.filters.selection.unfiled = unfiled;
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = root.join("managed-sidecar-fixture").exists();
    }
    let enum_hash: Arc<str> = Arc::from(download::compute_config_hash(&config));
    let store: Arc<dyn download::DownloadStore> = db;
    let base = make_run_cycle_download_config_builder(&media, store.clone());
    let build = |mode, exclude, groups, zone| {
        let mut cfg = base(mode, exclude, groups, zone);
        let cfg = Arc::make_mut(&mut cfg);
        cfg.edited_naming = naming;
        cfg.folder_structure = "unfiled".into();
        cfg.folder_structure_albums = Arc::from(album_template);
        cfg.enum_config_hash = Some(enum_hash.clone());
        #[cfg(feature = "xmp")]
        {
            cfg.metadata.xmp_sidecar = config.metadata.xmp_sidecar;
        }
        Arc::new(cfg.clone())
    };
    let (_session_root, session) = make_shared_session_for_run_cycle().await;
    Box::pin(crate::sync_cycle::run_cycle(
        &[&library],
        &config,
        Some(store.as_ref()),
        &build,
        download::DownloadControls::download_hidden(),
        &session,
        &provider.cancel,
    ))
    .await
    .unwrap()
}

async fn authenticated_library(provider: &Provider, db: Arc<state::SqliteStateDb>) -> PhotoLibrary {
    let mut library = PhotoLibrary::new(
        "https://example.com".into(),
        Arc::default(),
        Box::new(provider.clone()),
        Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db,
        fixture_owner(),
        "com",
    ));
    library
}

fn provider(records: Vec<Value>, members: Vec<&'static str>) -> Provider {
    Provider {
        records,
        members,
        pages: Vec::new(),
        interrupt: false,
        cancel: CancellationToken::new(),
    }
}

fn rewind(root: &std::path::Path) {
    rusqlite::Connection::open(root.join("state.db"))
        .unwrap()
        .execute(
            "UPDATE metadata SET value='before-departure' WHERE key='sync_token:PrimarySync'",
            [],
        )
        .unwrap();
}

async fn departure_preserves_peer(naming: crate::types::EditedNaming) {
    for local_change in ["unchanged", "media", "sidecar"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let records = records(&server, None);
        let child = records[1]["recordName"].as_str().unwrap().to_owned();
        let mut provider = provider(records, vec!["A", "B"]);
        cycle_with_naming(root.path(), provider.clone(), true, true, naming).await;
        let a = root.path().join("media/A/IMG_0501.JPG");
        let b = root.path().join("media/B/IMG_0501.JPG");
        assert_bytes(&a, ORIGINAL);
        assert_bytes(&b, ORIGINAL);
        let receipts = native_publication_receipts(root.path());
        assert!(!receipts.is_empty());
        if naming == crate::types::EditedNaming::Suffix {
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                0
            );
            assert_eq!(
                db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
                0
            );
        }
        let foreign = root.path().join("media/A/user.jpg");
        std::fs::write(&foreign, b"foreign").unwrap();
        let sidecar = a.with_file_name("IMG_0501.JPG.xmp");
        if local_change == "media" {
            std::fs::write(&a, b"local edit").unwrap();
        }
        if local_change == "sidecar" {
            std::fs::write(&sidecar, b"user sidecar").unwrap();
        }
        provider.members = vec!["B"];
        provider.pages = vec![vec![relation(&child, "A", true)]];
        rewind(root.path());
        cycle_with_naming(root.path(), provider.clone(), true, true, naming).await;
        assert_bytes(&b, ORIGINAL);
        assert_bytes(&foreign, b"foreign");
        match local_change {
            "unchanged" => assert!(!a.exists()),
            "media" => assert_bytes(&a, b"local edit"),
            _ => {
                assert_bytes(&a, ORIGINAL);
                assert_bytes(&sidecar, b"user sidecar");
            }
        }
        assert!(!root.path().join("media/unfiled/IMG_0501.JPG").exists());
        let current_receipts = native_publication_receipts(root.path());
        assert!(
            receipts
                .iter()
                .all(|receipt| current_receipts.contains(receipt))
        );
        let native = naming == crate::types::EditedNaming::Suffix;
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%' AND phase='committed'"
            ),
            if native {
                i64::from(local_change == "unchanged")
            } else {
                1
            }
        );
        if native {
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM provider_active_destinations WHERE current_publication=1 AND compat_path LIKE '%/A/%'"
                ),
                0
            );
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM provider_active_destinations WHERE current_publication=1 AND compat_path LIKE '%/B/%'"
                ),
                1
            );
        }
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        let requests = server.received_requests().await.unwrap().len();
        provider.pages = vec![Vec::new()];
        for _ in 0..2 {
            cycle_with_naming(root.path(), provider.clone(), true, true, naming).await;
            assert_bytes(&b, ORIGINAL);
            assert_bytes(&foreign, b"foreign");
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                operations
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            match local_change {
                "unchanged" => assert!(!a.exists()),
                "media" => assert_bytes(&a, b"local edit"),
                _ => {
                    assert_bytes(&a, ORIGINAL);
                    assert_bytes(&sidecar, b"user sidecar");
                }
            }
        }
    }
}

#[tokio::test]
async fn membership_native_suffix_departure_seeds_only_a_retirement_journal() {
    departure_preserves_peer(crate::types::EditedNaming::Suffix).await;
}

#[tokio::test]
async fn membership_departure_keeps_remaining_copy_and_releases_modified_departed_copy() {
    departure_preserves_peer(crate::types::EditedNaming::Primary).await;
}

#[tokio::test]
async fn membership_last_departure_uses_only_enabled_unfiled_destination() {
    for unfiled in [true, false] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let records = records(&server, None);
        let child = records[1]["recordName"].as_str().unwrap().to_owned();
        let mut provider = provider(records, vec!["A"]);
        cycle(root.path(), provider.clone(), unfiled).await;
        let old = root.path().join("media/A/IMG_0501.JPG");
        assert_bytes(&old, ORIGINAL);
        provider.members.clear();
        provider.pages = vec![vec![relation(&child, "A", true)]];
        rewind(root.path());
        cycle(root.path(), provider, unfiled).await;
        assert!(!old.exists());
        let fallback = root.path().join("media/unfiled/IMG_0501.JPG");
        if unfiled {
            assert_bytes(&fallback, ORIGINAL);
        } else {
            assert!(!fallback.exists());
        }
    }
}

#[tokio::test]
async fn membership_cross_page_move_interrupted_reopens_without_transient_fallback() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let records = records(&server, None);
    let child = records[1]["recordName"].as_str().unwrap().to_owned();
    let mut provider = provider(records, vec!["A"]);
    cycle(root.path(), provider.clone(), true).await;
    provider.members = vec!["B"];
    provider.pages = vec![
        vec![relation(&child, "A", true)],
        vec![relation(&child, "B", false)],
    ];
    provider.interrupt = true;
    rewind(root.path());
    cycle(root.path(), provider.clone(), true).await;
    assert_bytes(&root.path().join("media/A/IMG_0501.JPG"), ORIGINAL);
    assert!(!root.path().join("media/unfiled/IMG_0501.JPG").exists());
    provider.interrupt = false;
    provider.cancel = CancellationToken::new();
    cycle(root.path(), provider, true).await;
    assert!(!root.path().join("media/A/IMG_0501.JPG").exists());
    assert_bytes(&root.path().join("media/B/IMG_0501.JPG"), ORIGINAL);
    assert!(!root.path().join("media/unfiled/IMG_0501.JPG").exists());
}

#[tokio::test]
async fn membership_container_tombstone_redistributes_without_relation_tombstone() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut provider = provider(records(&server, None), vec!["A"]);
    cycle(root.path(), provider.clone(), true).await;
    let foreign = root.path().join("media/A/foreign.xmp");
    std::fs::write(&foreign, b"user metadata").unwrap();
    provider.members.clear();
    provider.pages = vec![vec![
        json!({"recordName":"A","recordType":"CPLAlbum","deleted":true}),
    ]];
    rewind(root.path());
    cycle_with_scope(root.path(), provider, true, false).await;
    assert!(!root.path().join("media/A/IMG_0501.JPG").exists());
    assert_bytes(&root.path().join("media/unfiled/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&foreign, b"user metadata");
}

#[tokio::test]
async fn membership_missing_album_discovery_does_not_authorize_retirement() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut provider = provider(records(&server, None), vec!["A", "B"]);
    cycle(root.path(), provider.clone(), true).await;
    provider.members = vec!["B"];
    provider.pages = vec![Vec::new()];
    rewind(root.path());
    cycle_with_scope(root.path(), provider, true, false).await;
    assert_bytes(&root.path().join("media/A/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&root.path().join("media/B/IMG_0501.JPG"), ORIGINAL);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%'"
        ),
        0
    );
}

#[tokio::test]
async fn membership_unparsable_departure_does_not_publish_a_fallback() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut provider = provider(records(&server, None), vec!["A"]);
    cycle(root.path(), provider.clone(), true).await;
    provider.members.clear();
    provider.pages = vec![vec![
        json!({"recordName":"unknown-relation","recordType":"CPLContainerRelation","deleted":true}),
    ]];
    rewind(root.path());
    cycle(root.path(), provider, true).await;
    assert_bytes(&root.path().join("media/A/IMG_0501.JPG"), ORIGINAL);
    assert!(!root.path().join("media/unfiled/IMG_0501.JPG").exists());
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%'"
        ),
        0
    );
}

#[tokio::test]
async fn membership_unknown_photo_identity_holds_departure_until_fresh_reopen() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let records = records(&server, None);
    let child = records[1]["recordName"].as_str().unwrap().to_owned();
    let mut provider = provider(records.clone(), vec!["A"]);
    cycle(root.path(), provider.clone(), true).await;
    provider.members.clear();
    provider.records.clear();
    provider.pages = vec![vec![relation(&child, "A", true)]];
    rewind(root.path());
    cycle(root.path(), provider.clone(), true).await;
    assert_bytes(&root.path().join("media/A/IMG_0501.JPG"), ORIGINAL);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%'"
        ),
        0
    );
    provider.records = records;
    cycle(root.path(), provider, true).await;
    assert!(!root.path().join("media/A/IMG_0501.JPG").exists());
    assert_bytes(&root.path().join("media/unfiled/IMG_0501.JPG"), ORIGINAL);
}

#[tokio::test]
async fn membership_whole_photo_deletion_preserves_all_existing_copies() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let records = records(&server, None);
    let child = records[1]["recordName"].as_str().unwrap().to_owned();
    let mut provider = provider(records, vec!["A", "B"]);
    cycle(root.path(), provider.clone(), true).await;
    provider.members.clear();
    provider.records.clear();
    provider.pages = vec![vec![
        relation(&child, "A", true),
        relation(&child, "B", true),
        json!({"recordName":child,"recordType":"CPLAsset","deleted":true}),
    ]];
    rewind(root.path());
    cycle(root.path(), provider, true).await;
    assert_bytes(&root.path().join("media/A/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&root.path().join("media/B/IMG_0501.JPG"), ORIGINAL);
    assert!(!root.path().join("media/unfiled/IMG_0501.JPG").exists());
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%'"
        ),
        0
    );
}

#[tokio::test]
async fn membership_preserved_local_matching_bytes_never_authorize_retirement() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let records = records(&server, None);
    let child = records[1]["recordName"].as_str().unwrap().to_owned();
    let mut provider = provider(records, vec!["A", "B"]);
    cycle_with_naming(
        root.path(),
        provider.clone(),
        true,
        true,
        crate::types::EditedNaming::Suffix,
    )
    .await;
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let (generation, key, path, checksum): (String,String,String,String) = conn.query_row("SELECT generation,pass_key,path,local_checksum FROM provider_active_destinations WHERE current_publication=1 AND compat_path LIKE '%/A/%'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    drop(conn);
    let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &fixture_owner())
        .await
        .unwrap();
    db.preserve_selection_local(
        fixture_owner(),
        generation,
        key,
        child.clone(),
        serde_json::from_str(&path).unwrap(),
        (checksum, ORIGINAL.len() as u64, None),
    )
    .await
    .unwrap();
    drop(db);
    provider.members = vec!["B"];
    provider.pages = vec![vec![relation(&child, "A", true)]];
    rewind(root.path());
    for _ in 0..3 {
        cycle_with_naming(
            root.path(),
            provider.clone(),
            true,
            true,
            crate::types::EditedNaming::Suffix,
        )
        .await;
        assert_bytes(&root.path().join("media/A/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&root.path().join("media/B/IMG_0501.JPG"), ORIGINAL);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_destinations WHERE compat_path LIKE '%/A/%' AND preserved_local IS NOT NULL AND current_publication=0 AND verified_media=0 AND source_checksum IS NULL"
            ),
            1
        );
        provider.pages = vec![Vec::new()];
    }
}

async fn deleted_selection_watch_wakes(
    root: &std::path::Path,
    provider: &Provider,
    binding: &state::db::provider_generations::BoundAlbumSelection,
    selection_config: &str,
) -> bool {
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("state.db"), &fixture_owner())
            .await
            .unwrap(),
    );
    let mut library = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        Vec::new(),
    );
    library.library = authenticated_library(provider, db.clone()).await;
    library.deleted_album_bindings = vec![binding.clone()];
    let media = root.join("media");
    let store: Arc<dyn download::DownloadStore> = db;
    let base = make_run_cycle_download_config_builder(&media, store);
    let build = |mode, exclude, groups, zone| {
        let mut config = base(mode, exclude, groups, zone);
        Arc::make_mut(&mut config).enum_config_hash = Some(Arc::from(selection_config));
        config
    };
    let mut check = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
    crate::sync_loop::precheck::include_pending_provider_work(
        &mut check,
        &[library],
        &build,
        download::DownloadControls::download_hidden(),
    )
    .await;
    check.should_sync_zone("PrimarySync")
}

#[tokio::test]
async fn membership_deleted_bound_zero_pass_recovery_keeps_provider_checkpoint() {
    for unavailable in [false, true] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let original = records(&server, None);
        let mut provider = provider(original.clone(), vec!["A"]);
        cycle_selected(
            root.path(),
            provider.clone(),
            &["A"],
            crate::types::EditedNaming::Suffix,
            "{album}",
            Vec::new(),
        )
        .await;
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let generation:String=conn.query_row("SELECT generation FROM provider_active_destinations WHERE verified_media=1 LIMIT 1",[],|row|row.get(0)).unwrap();
        let checkpoint: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let components:String=conn.query_row("SELECT json_extract(specification,'$.profile.passes[0].selector_components') FROM provider_active_generations WHERE id=?1",[&generation],|row|row.get(0)).unwrap();
        assert_eq!(components, "[\"A\"]");
        let specification: Vec<u8> = conn
            .query_row(
                "SELECT specification FROM provider_active_generations WHERE id=?1",
                [&generation],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let binding = state::db::provider_generations::BoundAlbumSelection {
            generation,
            container_id: "A".into(),
            components: vec!["A".into()],
            deleted: true,
        };
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &fixture_owner())
            .await
            .unwrap();
        let mut empty: state::db::provider_generations::GenerationSpec =
            serde_json::from_slice(&specification).unwrap();
        empty.profile["passes"] = json!([]);
        empty.profile["deleted_album_bindings"] = json!([binding]);
        assert!(
            db.begin_selection_generation(
                fixture_owner(),
                empty.clone(),
                state::db::provider_generations::MAX_GENERATION_BYTES
            )
            .await
            .is_err(),
            "caller flag cannot replace durable tombstone"
        );
        db.mark_album_container_deleted("PrimarySync", "A")
            .await
            .unwrap();
        db.invalidate_album_membership_snapshot("PrimarySync", "A")
            .await
            .unwrap();
        for drift in ["components", "config"] {
            let mut invalid = empty.clone();
            if drift == "components" {
                invalid.profile["deleted_album_bindings"][0]["components"] =
                    json!(["another selector"])
            } else {
                invalid.profile["selection_config"] = json!("changed-selection")
            }
            assert!(
                db.begin_selection_generation(
                    fixture_owner(),
                    invalid,
                    state::db::provider_generations::MAX_GENERATION_BYTES
                )
                .await
                .is_err(),
                "{drift}"
            );
        }
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_generations WHERE json_array_length(specification,'$.profile.passes')=0"
            ),
            0
        );
        drop(db);
        provider.members.clear();
        provider.pages = vec![Vec::new()];
        if unavailable {
            provider.records.clear();
        }
        let selection_config = empty.profile["selection_config"].as_str().unwrap();
        assert!(
            deleted_selection_watch_wakes(root.path(), &provider, &binding, selection_config).await
        );
        let result = cycle_selected(
            root.path(),
            provider.clone(),
            &[],
            crate::types::EditedNaming::Suffix,
            "{album}",
            vec![binding.clone()],
        )
        .await;
        assert!(!result.db_sync_token_advance_safe);
        if unavailable {
            assert!(result.failed_count > 0);
            assert_bytes(&root.path().join("media/A/IMG_0501.JPG"), ORIGINAL);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                0
            );
            provider.records = original;
            cycle_selected(
                root.path(),
                provider.clone(),
                &[],
                crate::types::EditedNaming::Suffix,
                "{album}",
                vec![binding.clone()],
            )
            .await;
        }
        assert!(!root.path().join("media/A/IMG_0501.JPG").exists());
        assert!(!root.path().join("media/unfiled").exists());
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='committed'"
            ),
            1
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_generations WHERE json_array_length(specification,'$.profile.passes')=0"
            ),
            1
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_selection_rank_pages p JOIN provider_active_generations g ON g.id=p.generation WHERE json_array_length(g.specification,'$.profile.passes')=0"
            ),
            0
        );
        let requests = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            assert!(
                !deleted_selection_watch_wakes(root.path(), &provider, &binding, selection_config)
                    .await
            );
            let quiet = cycle_selected(
                root.path(),
                provider.clone(),
                &[],
                crate::types::EditedNaming::Suffix,
                "{album}",
                vec![binding.clone()],
            )
            .await;
            assert!(!quiet.db_sync_token_advance_safe);
            assert_eq!(quiet.failed_count, 0);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                1
            );
        }
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            checkpoint
        );
        let recovery:String=conn.query_row("SELECT id FROM provider_active_generations WHERE json_array_length(specification,'$.profile.passes')=0",[],|row|row.get(0)).unwrap();
        drop(conn);
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &fixture_owner())
            .await
            .unwrap();
        db.upsert_album_container("PrimarySync", "A", "A", "album")
            .await
            .unwrap();
        assert!(
            db.current_selection_generation(fixture_owner(), empty.scope, empty.config_hash)
                .await
                .is_ok()
        );
        assert!(
            db.seal_selection_generation(fixture_owner(), recovery, true, None)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn membership_durable_container_tombstone_recovers_without_current_delta() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut provider = provider(records(&server, None), vec!["A", "B"]);
    cycle_with_naming(
        root.path(),
        provider.clone(),
        true,
        true,
        crate::types::EditedNaming::Suffix,
    )
    .await;
    let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &fixture_owner())
        .await
        .unwrap();
    db.mark_album_container_deleted("PrimarySync", "A")
        .await
        .unwrap();
    db.invalidate_album_membership_snapshot("PrimarySync", "A")
        .await
        .unwrap();
    drop(db);
    provider.members = vec!["B"];
    provider.pages = vec![Vec::new()];
    cycle_with_naming(
        root.path(),
        provider.clone(),
        true,
        false,
        crate::types::EditedNaming::Suffix,
    )
    .await;
    assert!(!root.path().join("media/A/IMG_0501.JPG").exists());
    assert_bytes(&root.path().join("media/B/IMG_0501.JPG"), ORIGINAL);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='committed'"
        ),
        1
    );
    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        cycle_with_naming(
            root.path(),
            provider.clone(),
            true,
            false,
            crate::types::EditedNaming::Suffix,
        )
        .await;
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            1
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "spawned by native membership process-death test"]
async fn membership_native_death_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    let records = super::records_at(&std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap(), None);
    let child = records[1]["recordName"].as_str().unwrap().to_owned();
    let mut provider = provider(records, vec!["B"]);
    provider.pages = vec![vec![relation(&child, "A", true)]];
    cycle_with_naming(
        &root,
        provider,
        true,
        true,
        if root.join("managed-sidecar-fixture").exists() {
            crate::types::EditedNaming::Primary
        } else {
            crate::types::EditedNaming::Suffix
        },
    )
    .await;
    panic!("native membership worker missed its death point");
}

#[cfg(target_os = "linux")]
async fn kill_sidecar_retirement_worker(root: &std::path::Path, url: &str, point: &str) {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    std::fs::write(root.join("fixture-owner"), b"kei-synthetic-process-death").unwrap();
    let log = std::fs::File::create(root.join("worker.log")).unwrap();
    let mut worker = Worker(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sync_loop::runner::tests::primary_layout::membership::membership_native_death_worker", "--ignored", "--nocapture"])
        .env("KEI_TEST_PROCESS_DEATH_ROOT", root).env("KEI_TEST_PROCESS_DEATH_POINT", point)
        .env("KEI_TEST_PROCESS_DEATH_URL", url)
        .stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log)).spawn().unwrap());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !root.join("ready").exists() {
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "{point}: {}",
            std::fs::read_to_string(root.join("worker.log")).unwrap()
        );
        assert!(
            std::time::Instant::now() < deadline,
            "sidecar retirement worker timeout: {point}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    worker.0.kill().unwrap();
    assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
}

#[cfg(target_os = "linux")]
async fn late_sidecar_retirement_fixture(naming: crate::types::EditedNaming) {
    let managed = naming == crate::types::EditedNaming::Primary;
    let server = server().await;
    for point in [
        "membership-layout-preserved",
        "layout-retirement-media-moved",
    ] {
        for changed in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            if managed {
                std::fs::write(root.join("managed-sidecar-fixture"), b"enable owned XMP").unwrap();
            }
            let records = records(&server, None);
            let child = records[1]["recordName"].as_str().unwrap().to_owned();
            let mut provider = provider(records, vec!["A", "B"]);
            cycle_with_naming(root, provider.clone(), true, true, naming).await;
            let media = root.join("media/A/IMG_0501.JPG");
            let sidecar = media.with_file_name("IMG_0501.JPG.xmp");
            let peer = root.join("media/B/IMG_0501.JPG");
            assert_bytes(&media, ORIGINAL);
            assert_eq!(sidecar.exists(), managed);
            let original_sidecar = managed.then(|| std::fs::read(&sidecar).unwrap());
            if managed {
                assert!(
                    db_count(
                        root,
                        "SELECT COUNT(*) FROM primary_layout_bindings WHERE json_type(binding,'$.files[0].sidecar')='object'"
                    ) > 0
                );
            } else {
                assert_eq!(
                    db_count(root, "SELECT COUNT(*) FROM primary_layout_operations"),
                    0
                );
            }
            let departed_receipts = || {
                let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
                conn.prepare("SELECT generation||pass_key||child||version_size||path||local_checksum||source_checksum||progress_hash FROM provider_active_destinations WHERE verified_media=1 AND compat_path LIKE '%/A/%' ORDER BY generation,pass_key,child,version_size,path").unwrap().query_map([],|row|row.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
            };
            let receipts = departed_receipts();
            assert!(!receipts.is_empty());
            rewind(root);
            kill_sidecar_retirement_worker(root, &server.uri(), point).await;
            assert_eq!(media.exists(), point == "membership-layout-preserved");
            if managed {
                assert_eq!(
                    std::fs::read(&sidecar).unwrap(),
                    *original_sidecar.as_ref().unwrap()
                );
            }
            assert_eq!(
                db_count(
                    root,
                    "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%' AND phase NOT IN ('committed','cancelled')"
                ),
                1
            );
            if changed {
                std::fs::write(&sidecar, b"late user metadata after preservation").unwrap();
            }
            let checkpoint = || {
                rusqlite::Connection::open(root.join("state.db"))
                    .unwrap()
                    .query_row(
                        "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap()
            };
            let held_checkpoint = checkpoint();
            provider.members = vec!["B"];
            provider.pages = vec![vec![relation(&child, "A", true)]];
            let requests = server.received_requests().await.unwrap().len();
            for _ in 0..3 {
                cycle_with_naming(root, provider.clone(), true, true, naming).await;
                assert_bytes(&peer, ORIGINAL);
                assert_eq!(server.received_requests().await.unwrap().len(), requests);
                assert!(!root.join("media/unfiled/IMG_0501.JPG").exists());
                let now = departed_receipts();
                assert!(receipts.iter().all(|receipt| now.contains(receipt)));
                if changed {
                    assert_bytes(&media, ORIGINAL);
                    assert_bytes(&sidecar, b"late user metadata after preservation");
                    assert_eq!(checkpoint(), held_checkpoint);
                    assert_eq!(
                        db_count(
                            root,
                            "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%' AND phase NOT IN ('committed','cancelled')"
                        ),
                        1
                    );
                    assert_eq!(
                        db_count(
                            root,
                            "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%' AND phase='committed'"
                        ),
                        0
                    );
                    assert!(
                        db_count(
                            root,
                            "SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
                        ) > 0
                    );
                    if !managed {
                        assert_eq!(
                            db_count(
                                root,
                                "SELECT COUNT(*) FROM provider_active_destinations WHERE compat_path LIKE '%/A/%' AND current_publication=1"
                            ),
                            1
                        );
                    }
                } else {
                    assert!(!media.exists());
                    assert!(!sidecar.exists());
                    assert_eq!(
                        db_count(
                            root,
                            "SELECT COUNT(*) FROM primary_layout_operations WHERE decision LIKE 'membership-departure:%' AND phase='committed'"
                        ),
                        1
                    );
                    assert_eq!(
                        db_count(
                            root,
                            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
                        ),
                        0
                    );
                    provider.pages = vec![Vec::new()];
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn membership_native_late_sidecar_preserves_pair_after_process_death() {
    late_sidecar_retirement_fixture(crate::types::EditedNaming::Suffix).await;
}

#[cfg(all(target_os = "linux", feature = "xmp"))]
#[tokio::test]
async fn membership_managed_late_sidecar_preserves_pair_after_process_death() {
    late_sidecar_retirement_fixture(crate::types::EditedNaming::Primary).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn membership_native_process_death_reopens_non_utf8_destinations() {
    use std::os::unix::{ffi::OsStringExt, process::ExitStatusExt};
    use std::process::{Command, Stdio};
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let server = server().await;
    for (point, preserve_after_crash) in [
        ("layout-preserved", false),
        ("layout-member-retired", false),
        ("layout-preserved", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp
            .path()
            .join(std::ffi::OsString::from_vec(b"native-\xff".to_vec()));
        std::fs::create_dir(&root).unwrap();
        let records = records(&server, None);
        let child = records[1]["recordName"].as_str().unwrap().to_owned();
        let mut provider = provider(records, vec!["A", "B"]);
        cycle_with_naming(
            &root,
            provider.clone(),
            true,
            true,
            crate::types::EditedNaming::Suffix,
        )
        .await;
        assert_eq!(
            db_count(&root, "SELECT COUNT(*) FROM primary_layout_operations"),
            0
        );
        assert_bytes(&root.join("media/A/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&root.join("media/B/IMG_0501.JPG"), ORIGINAL);
        rewind(&root);
        std::fs::write(root.join("fixture-owner"), b"kei-synthetic-process-death").unwrap();
        let log = std::fs::File::create(root.join("worker.log")).unwrap();
        let mut worker=Worker(Command::new(std::env::current_exe().unwrap()).args(["--exact","sync_loop::runner::tests::primary_layout::membership::membership_native_death_worker","--ignored","--nocapture"])
            .env("KEI_TEST_PROCESS_DEATH_ROOT",&root).env("KEI_TEST_PROCESS_DEATH_POINT",point).env("KEI_TEST_PROCESS_DEATH_URL",server.uri())
            .stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log)).spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !root.join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "{point}: {}",
                std::fs::read_to_string(root.join("worker.log")).unwrap()
            );
            assert!(
                std::time::Instant::now() < deadline,
                "worker timeout at {point}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        worker.0.kill().unwrap();
        assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
        provider.members = vec!["B"];
        provider.pages = vec![vec![relation(&child, "A", true)]];
        if preserve_after_crash {
            let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
            let native_path = crate::state::db::provider_selection::SelectionPath::from_path(
                &root.join("media/A/IMG_0501.JPG"),
            )
            .key()
            .unwrap();
            let (generation,key,path,checksum):(String,String,String,String)=conn.query_row("SELECT generation,pass_key,path,local_checksum FROM provider_active_destinations WHERE current_publication=1 AND path=?1",[native_path],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
            drop(conn);
            let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &fixture_owner())
                .await
                .unwrap();
            db.preserve_selection_local(
                fixture_owner(),
                generation,
                key,
                child.clone(),
                serde_json::from_str(&path).unwrap(),
                (checksum, ORIGINAL.len() as u64, None),
            )
            .await
            .unwrap();
            drop(db);
            for _ in 0..2 {
                cycle_with_naming(
                    &root,
                    provider.clone(),
                    true,
                    true,
                    crate::types::EditedNaming::Suffix,
                )
                .await;
                assert_bytes(&root.join("media/A/IMG_0501.JPG"), ORIGINAL);
                assert_bytes(&root.join("media/B/IMG_0501.JPG"), ORIGINAL);
                assert_eq!(
                    db_count(
                        &root,
                        "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='committed'"
                    ),
                    0
                );
            }
            continue;
        }
        let requests = server.received_requests().await.unwrap().len();
        cycle_with_naming(
            &root,
            provider.clone(),
            true,
            true,
            crate::types::EditedNaming::Suffix,
        )
        .await;
        assert!(!root.join("media/A/IMG_0501.JPG").exists());
        assert_bytes(&root.join("media/B/IMG_0501.JPG"), ORIGINAL);
        assert_eq!(
            db_count(
                &root,
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='committed'"
            ),
            1
        );
        assert_eq!(
            db_count(
                &root,
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
            ),
            0
        );
        provider.pages = vec![Vec::new()];
        for _ in 0..2 {
            cycle_with_naming(
                &root,
                provider.clone(),
                true,
                true,
                crate::types::EditedNaming::Suffix,
            )
            .await;
            assert!(!root.join("media/A/IMG_0501.JPG").exists());
            assert_bytes(&root.join("media/B/IMG_0501.JPG"), ORIGINAL);
            assert_eq!(
                db_count(&root, "SELECT COUNT(*) FROM primary_layout_operations"),
                1
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }
    }
}
