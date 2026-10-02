//! Two fixed six-cycle histories, observed through equivalent provider encodings.
//! Replays retain the entire prerequisite/recovery/quiet tail; no unsafe reducer.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};

const ZONES: [&str; 2] = ["PrimarySync", "SharedSync-MIXED"];
const CYCLES: usize = 6;
const STILL: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
const MOTION: &[u8] = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum History {
    IncompleteInventory,
    SelectionChange,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Encoding {
    Baseline,
    PageCuts,
    DuplicateEmpty,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Replay {
    history: History,
    encoding: Encoding,
}

#[derive(Clone, Debug)]
struct Provider {
    zone: &'static str,
    records: Vec<Value>,
    query_records: Vec<Value>,
    encoding: Encoding,
    incomplete: bool,
    quiet: bool,
    requests: Arc<AtomicUsize>,
    hydrations: Arc<AtomicUsize>,
}
impl Provider {
    fn pages(&self) -> Vec<Vec<Value>> {
        // All observations are snapshots in this fixture, with one version per
        // record. Reorder independent families only; never sort causal updates.
        let mut records = self.records.clone();
        if !matches!(self.encoding, Encoding::Baseline) {
            records.rotate_left(3);
        }
        if matches!(self.encoding, Encoding::DuplicateEmpty) {
            records = records
                .into_iter()
                .flat_map(|record| [record.clone(), record])
                .collect();
        }
        let width = if matches!(self.encoding, Encoding::Baseline) {
            records.len()
        } else {
            2
        };
        let mut pages: Vec<_> = records.chunks(width).map(<[Value]>::to_vec).collect();
        if matches!(self.encoding, Encoding::DuplicateEmpty) {
            pages.insert(1, Vec::new());
        }
        pages
    }
}
#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let count = self.requests.fetch_add(1, Ordering::SeqCst);
        assert!(count < 300, "bounded provider work per history");
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"]["zoneName"], self.zone);
            let cursor = request["zones"][0]["syncToken"].as_str();
            let inventory = cursor.is_none() || cursor.is_some_and(|s| s.starts_with("page-"));
            let pages = self.pages();
            let index = cursor
                .and_then(|s| s.strip_prefix("page-"))
                .map_or(0, |s| s.parse::<usize>().unwrap());
            let final_page = !inventory || index + 1 == pages.len();
            let records = if inventory {
                pages[index].clone()
            } else {
                Vec::new()
            };
            let token = if final_page {
                "after".to_string()
            } else {
                format!("page-{}", index + 1)
            };
            let mut response = json!({"zones":[{"zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},"records":records,"syncToken":token,"moreComing":!final_page}]});
            if inventory && final_page && self.incomplete {
                // The final page is unavailable, not an authoritative empty EOF.
                response["zones"][0]["records"] = json!([]);
                response["zones"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("moreComing");
            }
            return Ok(response);
        }
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response((self.query_records.len() / 2) as u64));
        }
        if url.contains("/records/query?") {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset == 0 {self.query_records.clone()} else {Vec::new()},"syncToken":"after"}),
            );
        }
        assert!(
            url.contains("/records/lookup?"),
            "unexpected provider route {url}"
        );
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        assert!(!self.quiet, "quiet tail must not hydrate");
        assert_eq!(request["zoneID"]["zoneName"], self.zone);
        let requested = request["records"].as_array().unwrap();
        let records: Vec<_> = self
            .records
            .iter()
            .filter(|record| {
                requested
                    .iter()
                    .any(|r| r["recordName"] == record["recordName"])
            })
            .cloned()
            .collect();
        Ok(json!({"records":records}))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn album(provider: Provider, name: &str, container: Option<&str>) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.invalid"),
            name: Arc::from(name),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":provider.zone})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: container.map(Arc::from),
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider),
    )
}
fn checksum(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}
fn local_hash(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}
fn date(zone: usize) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS + zone as i64 * 86_400_000)
        .unwrap()
}
fn date_dir(zone: usize) -> String {
    date(zone)
        .with_timezone(&chrono::Local)
        .format("%Y/%m/%d")
        .to_string()
}

async fn seed_file(
    db: &state::SqliteStateDb,
    zone: usize,
    id: &str,
    version: state::VersionSizeKey,
    target: &Path,
    bytes: &[u8],
) {
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, bytes).unwrap();
    let mut metadata = state::AssetMetadata::default();
    if id != "legacy-master" {
        if version == state::VersionSizeKey::Original {
            metadata.width = Some(100);
            metadata.height = Some(100);
        }
        metadata.provider_data =
            Some(json!({"adjustmentRenderType":{"type":"INT64","value":0}}).to_string());
        metadata.refresh_hash();
    }
    let row = crate::test_helpers::TestAssetRecord::new(id)
        .library(ZONES[zone])
        .metadata(metadata)
        .version_size(version)
        .filename(if version == state::VersionSizeKey::Original {
            "photo.JPG"
        } else {
            "photo.MOV"
        })
        .created_at(date(zone))
        .added_at(date(zone))
        .size(bytes.len() as u64)
        .checksum(&checksum(bytes))
        .build();
    db.upsert_seen(&row).await.unwrap();
    let hash = local_hash(bytes);
    db.mark_downloaded(
        ZONES[zone],
        id,
        version.as_str(),
        target,
        &hash,
        Some(&hash),
    )
    .await
    .unwrap();
}
fn rows(database: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(database).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

// Normalize only transport/runtime accidents: wall times, temporary root, and
// newly allocated suffix spelling. Identity, rendition, bytes, receipt count,
// retry existence, deletion and checkpoints remain in the comparison.
fn snapshot(database: &Path) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    [
        "SELECT library,id,version_size,checksum,created_at,added_at,status,is_favorite FROM assets ORDER BY 1,2,3",
        "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings ORDER BY 1,2",
        "SELECT library,master_record_name,asset_record_name FROM legacy_master_state_owners ORDER BY 1,2",
        "SELECT library,id,version_size,provider_checksum,local_checksum,count(*) FROM asset_metadata_paths GROUP BY 1,2,3,4,5 ORDER BY 1,2,3,4,5",
        "SELECT library,asset_id,revision FROM asset_metadata_capture_revisions ORDER BY 1,2",
        "SELECT library,asset_id FROM metadata_capture_retries ORDER BY 1,2",
        "SELECT library,asset_id,active_generation FROM unattributed_legacy ORDER BY 1,2",
        "SELECT library,asset_id,generation,prior_cursor,next_cursor FROM unattributed_legacy_proofs ORDER BY 1,2,3",
        "SELECT library,asset_record_name,master_record_name,container_id,is_deleted FROM asset_album_memberships ORDER BY 1,2,4",
        "SELECT key,value FROM metadata WHERE key LIKE 'sync_token:%' OR key LIKE 'pending_sync_token:%' OR key LIKE 'unresolved_asset_identity:%' ORDER BY key",
    ].iter().map(|sql| rows(database, sql)).collect()
}

async fn run_history(replay: Replay) -> Vec<Vec<Vec<Vec<rusqlite::types::Value>>>> {
    eprintln!(
        "KEI_MIXED_REPLAY='{}' cargo test --lib mixed_library_provider_shapes -- --nocapture",
        serde_json::to_string(&replay).unwrap()
    );
    // A bind failure is a real failure, never a silently skipped proof.
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    let mut records_by_zone = Vec::new();
    let mut bytes_by_zone = Vec::new();
    let mut seeded_paths = BTreeMap::new();
    {
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        for (zone, name) in ZONES.iter().enumerate() {
            let mut bytes = STILL.to_vec();
            bytes.push(zone as u8);
            let mut motion = MOTION.to_vec();
            motion.extend([zone as u8; 4]);
            for (kind, content) in [("jpg", &bytes), ("mov", &motion)] {
                Mock::given(method("GET"))
                    .and(path(format!("/{zone}.{kind}")))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
                    .mount(&server)
                    .await;
            }
            let mut records = Vec::new();
            for master in ["legacy-master", "owned-master"] {
                let mut page = full_album_page_with_download(
                    name,
                    master,
                    "after",
                    &format!("{}/{zone}.jpg", server.uri()),
                    bytes.len() as u64,
                    &checksum(&bytes),
                );
                page["records"][0]["fields"]["filenameEnc"] =
                    json!({"value":"photo.JPG","type":"STRING"});
                page["records"][1]["fields"]["assetDate"]["value"] =
                    json!(date(zone).timestamp_millis());
                page["records"][1]["fields"]["addedDate"]["value"] =
                    json!(date(zone).timestamp_millis());
                if master == "legacy-master" {
                    page["records"][1]["recordName"] = json!("current-a");
                    let mut sibling = page["records"][1].clone();
                    sibling["recordName"] = json!("current-b");
                    page["records"].as_array_mut().unwrap().push(sibling);
                } else {
                    page["records"][0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/{zone}.mov",server.uri()),"size":motion.len(),"fileChecksum":checksum(&motion)}});
                    page["records"][0]["fields"]["resOriginalVidComplFileType"] =
                        json!({"value":"com.apple.quicktime-movie"});
                }
                records.extend(page["records"].as_array().unwrap().clone());
            }
            let legacy = media.join(date_dir(zone)).join("photo.JPG");
            seed_file(
                &db,
                zone,
                "legacy-master",
                state::VersionSizeKey::Original,
                &legacy,
                &bytes,
            )
            .await;
            let sidecar = legacy.with_extension("JPG.xmp");
            std::fs::write(&sidecar, b"unattributed private sidecar").unwrap();
            for child in ["current-a", "current-b"] {
                db.upsert_asset_master_mapping(name, child, "legacy-master")
                    .await
                    .unwrap();
            }
            db.set_metadata_capture_revision_for_test(name, "legacy-master", 0);
            db.begin_metadata_capture_revision(name, state::METADATA_CAPTURE_REVISION)
                .await
                .unwrap();
            let candidate = db
                .get_metadata_capture_candidates(name, 1, 1)
                .await
                .unwrap()
                .remove(0);
            assert!(
                db.defer_metadata_capture_ambiguity(&candidate, 1)
                    .await
                    .unwrap()
            );
            // A current child is independently owned before this history starts;
            // the legacy row and a later sibling must never absorb its receipt.
            let current = media.join(date_dir(zone)).join("photo-current-a-9.JPG");
            seed_file(
                &db,
                zone,
                "current-a",
                state::VersionSizeKey::Original,
                &current,
                &bytes,
            )
            .await;
            db.upsert_asset_master_mapping(name, "asset-owned-master", "owned-master")
                .await
                .unwrap();
            for label in ["B", "C"] {
                let album_name = format!("{name}-{label}");
                db.upsert_album_container(name, label, &album_name, "album")
                    .await
                    .unwrap();
                let generation = db
                    .start_album_membership_snapshot(name, label, None)
                    .await
                    .unwrap();
                db.add_album_membership_to_snapshot(
                    name,
                    label,
                    generation,
                    "asset-owned-master",
                    Some("owned-master"),
                    "icloud",
                )
                .await
                .unwrap();
                db.complete_album_membership_snapshot(name, label, generation)
                    .await
                    .unwrap();
            }
            // Membership predates these completed publication receipts.
            for label in ["B", "C"] {
                let album_name = format!("{name}-{label}");
                for (version, filename, content) in [
                    (
                        state::VersionSizeKey::Original,
                        "photo-asset-owned-master-33.JPG",
                        &bytes,
                    ),
                    (
                        state::VersionSizeKey::LiveOriginal,
                        "photo-asset-owned-master-33.MOV",
                        &motion,
                    ),
                ] {
                    let target = media.join(&album_name).join(filename);
                    seed_file(&db, zone, "asset-owned-master", version, &target, content).await;
                }
            }
            db.set_metadata(&format!("sync_token:{name}"), "before")
                .await
                .unwrap();
            records_by_zone.push(records);
            bytes_by_zone.push((bytes, motion));
        }
        seeded_paths.extend(files(&media));
    }
    let legacy_rows = rows(
        &database,
        "SELECT library,id,version_size,checksum,created_at,added_at,status,local_path FROM assets WHERE id='legacy-master' ORDER BY library",
    );
    let legacy_retries = rows(
        &database,
        "SELECT library,asset_id FROM metadata_capture_retries ORDER BY library",
    );
    let requests: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let hydrations: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut outputs = Vec::new();
    let mut stable_files = None;
    let mut stable_downloads = 0;
    let mut stable_hydrations = 0;
    let mut child_paths = BTreeMap::new();
    for cycle in 0..CYCLES {
        eprintln!("{replay:?} cycle={cycle}/{CYCLES}");
        let result = {
            let db: Arc<dyn download::DownloadStore> =
                Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
            let mut libraries = Vec::new();
            for (zone, name) in ZONES.iter().enumerate() {
                let mut records = records_by_zone[zone].clone();
                if zone == 0 && cycle < 2 && matches!(replay.history, History::SelectionChange) {
                    records[2]["fields"]
                        .as_object_mut()
                        .unwrap()
                        .remove("assetDate");
                }
                let provider = Provider {
                    zone: name,
                    query_records: records[..3].to_vec(),
                    records,
                    encoding: replay.encoding,
                    incomplete: zone == 0
                        && cycle < 2
                        && matches!(replay.history, History::IncompleteInventory),
                    quiet: cycle >= 4,
                    requests: requests[zone].clone(),
                    hydrations: hydrations[zone].clone(),
                };
                let labels: &[&str] = if matches!(replay.history, History::SelectionChange) {
                    match cycle {
                        0 => &["B"],
                        1 => &["C", "B"],
                        _ => &["B", "C"],
                    }
                } else {
                    &["B", "C"]
                };
                let mut passes = Vec::new();
                for label in labels {
                    let mut album_provider = provider.clone();
                    album_provider.query_records = records_by_zone[zone][3..].to_vec();
                    passes.push(AlbumPass {
                        kind: PassKind::Album,
                        album: album(album_provider, &format!("{name}-{label}"), Some(label)),
                        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                    });
                }
                passes.push(AlbumPass {
                    kind: PassKind::Unfiled,
                    album: album(provider.clone(), "", None),
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::from_iter([
                        "asset-owned-master".to_string()
                    ])),
                });
                let mut library = make_run_cycle_library_state_with_passes(
                    name,
                    &format!("sync_token:{name}"),
                    passes,
                );
                library.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                    Box::new(provider),
                    name,
                );
                libraries.push(library);
            }
            let mut config = make_run_cycle_config();
            let labels: &[&str] =
                if matches!(replay.history, History::SelectionChange) && cycle == 0 {
                    &["B"]
                } else {
                    &["B", "C"]
                };
            config.filters.selection.albums = crate::selection::AlbumSelector::Named {
                included: ZONES
                    .iter()
                    .flat_map(|zone| labels.iter().map(move |label| format!("{zone}-{label}")))
                    .collect(),
                excluded: Default::default(),
            };
            config.filters.selection.albums_explicit = true;
            config.photos.live_photo_mode =
                if matches!(replay.history, History::SelectionChange) && cycle == 0 {
                    crate::types::LivePhotoMode::ImageOnly
                } else {
                    crate::types::LivePhotoMode::Both
                };
            let base_build = make_run_cycle_download_config_builder_with_options(
                &media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: config.filters.media,
                    per_pass_paths: true,
                    concurrent_downloads: Some(1),
                    ..RunCycleDownloadConfigOptions::default()
                },
            );
            let build = |mode, excluded, groups, library| {
                let mut built = base_build(mode, excluded, groups, library);
                Arc::make_mut(&mut built).live_photo_mode = config.photos.live_photo_mode;
                built
            };
            crate::sync_cycle::run_cycle(
                &libraries.iter().collect::<Vec<_>>(),
                &config,
                Some(db.as_ref()),
                false,
                &build,
                download::DownloadControls::download_hidden(),
                &session,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
        };
        // All production handles are gone. Reopen independently after EVERY
        // cycle, including both quiet tail cycles and the final comparison.
        assert_eq!(
            rows(
                &database,
                "SELECT library,id,version_size,checksum,created_at,added_at,status,local_path FROM assets WHERE id='legacy-master' ORDER BY library"
            ),
            legacy_rows,
            "legacy receipt unchanged"
        );
        assert_eq!(
            rows(
                &database,
                "SELECT library,asset_id FROM metadata_capture_retries ORDER BY library"
            ),
            legacy_retries,
            "preserved retry evidence remains distinct from current children"
        );
        assert!(
            rows(&database, "SELECT * FROM legacy_master_state_owners").is_empty(),
            "inventory cannot invent legacy ownership"
        );
        for (path, content) in &seeded_paths {
            assert_eq!(
                &std::fs::read(media.join(path)).unwrap(),
                content,
                "owned and legacy bytes stable: {path:?}"
            );
        }
        let reopened = state::SqliteStateDb::open(&database).await.unwrap();
        for (zone, name) in ZONES.iter().enumerate() {
            let eligible = zone == 1 || cycle >= 2;
            assert_eq!(
                reopened
                    .get_metadata(&format!("sync_token:{name}"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some(if eligible { "after" } else { "before" }),
                "first eligible progress {replay:?} cycle={cycle} zone={name}: {:?}",
                result.stats
            );
            let downloaded = reopened.get_downloaded_page(0, 100).await.unwrap();
            let mut paths = HashSet::new();
            for child in ["current-a", "current-b"] {
                let found = downloaded
                    .iter()
                    .find(|r| r.library.as_ref() == *name && r.id.as_ref() == child);
                if eligible || child == "current-a" {
                    let row =
                        found.expect("each current sibling has an independent durable receipt");
                    assert_eq!(row.created_at, date(zone));
                    let target = row.local_path.as_ref().unwrap();
                    assert!(
                        paths.insert(target.clone()),
                        "siblings cannot share an owned path"
                    );
                    assert_eq!(
                        std::fs::read(target).unwrap(),
                        bytes_by_zone[zone].0,
                        "zone-specific bytes"
                    );
                    let key = (zone, child.to_string());
                    if let Some(previous) = child_paths.insert(key, target.clone()) {
                        assert_eq!(previous, *target, "subsequent collision path stability");
                    }
                    if child == "current-a" {
                        assert_eq!(
                            *target,
                            media.join(date_dir(zone)).join("photo-current-a-9.JPG"),
                            "existing independent child keeps its path"
                        );
                    }
                } else {
                    assert!(
                        found.is_none(),
                        "omitted final inventory cannot create child receipts"
                    );
                }
            }
        }
        assert!(
            rows(
                &database,
                "SELECT id FROM assets WHERE status IN ('pending','failed','source_deleted')"
            )
            .is_empty(),
            "no erased siblings or leaked transfers"
        );
        for (zone, name) in ZONES.iter().enumerate() {
            let owned = rows(
                &database,
                &format!(
                    "SELECT local_path FROM asset_metadata_paths WHERE library='{name}' AND id='asset-owned-master' ORDER BY local_path"
                ),
            );
            let expected: Vec<_> = seeded_paths
                .keys()
                .filter(|path| {
                    path.starts_with(format!("{name}-B")) || path.starts_with(format!("{name}-C"))
                })
                .map(|path| {
                    vec![rusqlite::types::Value::Text(
                        media.join(path).to_string_lossy().into_owned(),
                    )]
                })
                .collect();
            assert_eq!(
                owned, expected,
                "both numbered album publication receipts remain stable in zone {zone}"
            );
        }
        assert_eq!(
            files(&media).len(),
            seeded_paths.len() + if cycle >= 2 { 2 } else { 1 },
            "exact owned files; no new duplicate copies or stale temporary media"
        );
        let state = snapshot(&database);
        if cycle >= 4 {
            assert_eq!(result.stats.downloaded, 0, "quiet download count");
            assert_eq!(
                result.stats.metadata_capture_refreshed, 0,
                "quiet metadata repair count"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                stable_downloads,
                "quiet HTTP downloads"
            );
            assert_eq!(
                hydrations
                    .iter()
                    .map(|v| v.load(Ordering::SeqCst))
                    .sum::<usize>(),
                stable_hydrations,
                "quiet hydration count"
            );
            assert_eq!(
                files(&media),
                *stable_files.as_ref().unwrap(),
                "quiet file growth/bytes"
            );
            assert_eq!(state, outputs[3], "quiet durable state");
        }
        if cycle == 3 {
            stable_files = Some(files(&media));
            stable_downloads = server.received_requests().await.unwrap().len();
            stable_hydrations = hydrations.iter().map(|v| v.load(Ordering::SeqCst)).sum();
        }
        outputs.push(state);
    }
    assert!(
        server.received_requests().await.unwrap().len() <= 12,
        "bounded media work"
    );
    outputs
}

#[tokio::test]
async fn mixed_library_provider_shapes() {
    if let Ok(trace) = std::env::var("KEI_MIXED_REPLAY") {
        let replay: Replay = serde_json::from_str(&trace).expect("bounded typed replay JSON");
        let baseline = Box::pin(run_history(Replay {
            encoding: Encoding::Baseline,
            ..replay
        }))
        .await;
        if replay.encoding != Encoding::Baseline {
            let actual = Box::pin(run_history(replay)).await;
            assert_eq!(actual, baseline, "provider response equivalence on replay");
        }
        return;
    }
    for history in [History::IncompleteInventory, History::SelectionChange] {
        let baseline = Box::pin(run_history(Replay {
            history,
            encoding: Encoding::Baseline,
        }))
        .await;
        for encoding in [Encoding::PageCuts, Encoding::DuplicateEmpty] {
            let actual = Box::pin(run_history(Replay { history, encoding })).await;
            assert_eq!(
                actual, baseline,
                "provider response equivalence {history:?} {encoding:?}"
            );
        }
    }
}

// Reduction of SelectionChange: no legacy rows, identity debt or second zone.
#[tokio::test]
async fn mixed_numbered_album_mode_transition_repro() {
    let server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    let mut page = full_album_page_with_download(
        ZONES[0],
        "owned-master",
        "after",
        &format!("{}/still", server.uri()),
        STILL.len() as u64,
        &checksum(STILL),
    );
    page["records"][0]["fields"]["filenameEnc"] = json!({"value":"photo.JPG","type":"STRING"});
    page["records"][0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/motion",server.uri()),"size":MOTION.len(),"fileChecksum":checksum(MOTION)}});
    page["records"][0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    let records = page["records"].as_array().unwrap().clone();
    {
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        db.upsert_asset_master_mapping(ZONES[0], "asset-owned-master", "owned-master")
            .await
            .unwrap();
        for label in ["B", "C"] {
            db.upsert_album_container(ZONES[0], label, label, "album")
                .await
                .unwrap();
            let generation = db
                .start_album_membership_snapshot(ZONES[0], label, None)
                .await
                .unwrap();
            db.add_album_membership_to_snapshot(
                ZONES[0],
                label,
                generation,
                "asset-owned-master",
                Some("owned-master"),
                "icloud",
            )
            .await
            .unwrap();
            db.complete_album_membership_snapshot(ZONES[0], label, generation)
                .await
                .unwrap();
        }
        for label in ["B", "C"] {
            for (version, name, content) in [
                (
                    state::VersionSizeKey::Original,
                    "photo-asset-owned-master-33.JPG",
                    STILL,
                ),
                (
                    state::VersionSizeKey::LiveOriginal,
                    "photo-asset-owned-master-33.MOV",
                    MOTION,
                ),
            ] {
                seed_file(
                    &db,
                    0,
                    "asset-owned-master",
                    version,
                    &media.join(label).join(name),
                    content,
                )
                .await;
            }
        }
        db.set_metadata("sync_token:PrimarySync", "before")
            .await
            .unwrap();
    }
    let initial = files(&media);
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    for cycle in 0..2 {
        {
            let db: Arc<dyn download::DownloadStore> =
                Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
            let labels: &[&str] = if cycle == 0 { &["B"] } else { &["C", "B"] };
            let provider = Provider {
                zone: ZONES[0],
                records: records.clone(),
                query_records: records.clone(),
                encoding: Encoding::Baseline,
                incomplete: false,
                quiet: false,
                requests: Arc::new(AtomicUsize::new(0)),
                hydrations: Arc::new(AtomicUsize::new(0)),
            };
            let passes = labels
                .iter()
                .map(|label| AlbumPass {
                    kind: PassKind::Album,
                    album: album(provider.clone(), label, Some(label)),
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                })
                .collect();
            let mut library = make_run_cycle_library_state_with_passes(
                ZONES[0],
                "sync_token:PrimarySync",
                passes,
            );
            library.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(provider),
                ZONES[0],
            );
            let mut config = make_run_cycle_config();
            config.filters.selection.albums = crate::selection::AlbumSelector::Named {
                included: labels.iter().map(|s| s.to_string()).collect(),
                excluded: Default::default(),
            };
            config.filters.selection.albums_explicit = true;
            config.filters.selection.unfiled = false;
            config.photos.live_photo_mode = if cycle == 0 {
                crate::types::LivePhotoMode::ImageOnly
            } else {
                crate::types::LivePhotoMode::Both
            };
            let base = make_run_cycle_download_config_builder_with_options(
                &media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    per_pass_paths: true,
                    concurrent_downloads: Some(1),
                    ..Default::default()
                },
            );
            let build = |mode, excluded, groups, library| {
                let mut built = base(mode, excluded, groups, library);
                Arc::make_mut(&mut built).live_photo_mode = config.photos.live_photo_mode;
                built
            };
            let result = crate::sync_cycle::run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                false,
                &build,
                download::DownloadControls::download_hidden(),
                &session,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            eprintln!(
                "reduced cycle={cycle} downloaded={} files={:?}",
                result.stats.downloaded,
                files(&media).keys()
            );
        }
        assert_eq!(
            files(&media),
            initial,
            "ImageOnly to Both with existing numbered album receipts must not grow media files (cycle {cycle})"
        );
        assert_eq!(
            rows(&database, "SELECT count(*) FROM asset_metadata_paths"),
            vec![vec![rusqlite::types::Value::Integer(4)]],
            "exact existing publication receipts"
        );
    }
    server.verify().await;
}

#[test]
fn mixed_history_replay_round_trip() {
    for history in [History::IncompleteInventory, History::SelectionChange] {
        for encoding in [
            Encoding::Baseline,
            Encoding::PageCuts,
            Encoding::DuplicateEmpty,
        ] {
            let replay = Replay { history, encoding };
            let encoded = serde_json::to_string(&replay).unwrap();
            assert_eq!(serde_json::from_str::<Replay>(&encoded).unwrap(), replay);
        }
    }
    assert!(
        serde_json::from_str::<Replay>(
            r#"{"history":"IncompleteInventory","encoding":"Baseline","cycles":999}"#
        )
        .is_err()
    );
    assert_eq!(CYCLES, 6, "replay cannot extend the cycle cap");
}

// Reduction: one zone, no albums or Live Photo, one reconciliation call.
#[tokio::test]
async fn mixed_legacy_reservation_blocks_preparation_repro() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
    let mut page = full_album_page_with_download(
        ZONES[0],
        "legacy-master",
        "after",
        "http://127.0.0.1:1/unused",
        STILL.len() as u64,
        &checksum(STILL),
    );
    page["records"][0]["fields"]["filenameEnc"] = json!({"value":"photo.JPG","type":"STRING"});
    page["records"][1]["recordName"] = json!("current-a");
    page["records"][1]["fields"]["assetDate"]["value"] = json!(date(0).timestamp_millis());
    page["records"][1]["fields"]["addedDate"]["value"] = json!(date(0).timestamp_millis());
    let mut sibling = page["records"][1].clone();
    sibling["recordName"] = json!("current-b");
    sibling["fields"]
        .as_object_mut()
        .unwrap()
        .remove("assetDate");
    page["records"].as_array_mut().unwrap().push(sibling);
    for (id, filename) in [
        ("legacy-master", "photo.JPG"),
        ("current-a", "photo-current-a-9.JPG"),
    ] {
        seed_file(
            &db,
            0,
            id,
            state::VersionSizeKey::Original,
            &media.join(date_dir(0)).join(filename),
            STILL,
        )
        .await;
    }
    for child in ["current-a", "current-b"] {
        db.upsert_asset_master_mapping(ZONES[0], child, "legacy-master")
            .await
            .unwrap();
    }
    db.set_metadata_capture_revision_for_test(ZONES[0], "legacy-master", 0);
    db.begin_metadata_capture_revision(ZONES[0], state::METADATA_CAPTURE_REVISION)
        .await
        .unwrap();
    let candidate = db
        .get_metadata_capture_candidates(ZONES[0], 1, 1)
        .await
        .unwrap()
        .remove(0);
    assert!(
        db.defer_metadata_capture_ambiguity(&candidate, 1)
            .await
            .unwrap()
    );
    assert_eq!(
        db.legacy_preparation_snapshots(ZONES[0], 64)
            .await
            .unwrap()
            .len(),
        1,
        "initial preservation candidate"
    );
    let provider = Provider {
        zone: ZONES[0],
        records: page["records"].as_array().unwrap().clone(),
        query_records: vec![],
        encoding: Encoding::Baseline,
        incomplete: false,
        quiet: false,
        requests: Arc::new(AtomicUsize::new(0)),
        hydrations: Arc::new(AtomicUsize::new(0)),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album(provider, "", None),
        exclude_ids: Default::default(),
    }];
    let build = make_run_cycle_download_config_builder_with_options(
        &media,
        db.clone(),
        RunCycleDownloadConfigOptions::default(),
    );
    let config = build(
        download::SyncMode::Full,
        Arc::new(Default::default()),
        Arc::new(Default::default()),
        Arc::from(ZONES[0]),
    );
    let result =
        crate::download::reconcile_catalog_paths(&passes, config, CancellationToken::new())
            .await
            .unwrap();
    assert!(!result.complete, "unattributed identity remains unresolved");
    assert_eq!(result.stats.downloaded, 0, "independent receipt is reused");
    drop(build);
    drop(db);
    let reopened = state::SqliteStateDb::open(&database).await.unwrap();
    assert!(
        rows(
            &database,
            "SELECT * FROM reconciliation_paths WHERE id='legacy-master'"
        )
        .is_empty(),
        "no reservation for an unattributed master"
    );
    assert!(
        rows(&database, "SELECT * FROM legacy_master_state_owners").is_empty(),
        "lookup cannot claim an owner"
    );
    assert_eq!(files(&media).len(), 2, "no duplicate media");
    assert_eq!(
        std::fs::read(media.join(date_dir(0)).join("photo-current-a-9.JPG")).unwrap(),
        STILL,
        "independent bytes unchanged"
    );
    assert_eq!(
        std::fs::read(media.join(date_dir(0)).join("photo.JPG")).unwrap(),
        STILL
    );
    assert_eq!(
        reopened
            .legacy_preparation_snapshots(ZONES[0], 64)
            .await
            .unwrap()
            .len(),
        1,
        "reconciliation cannot disqualify an unattributed master while evidence remains incomplete"
    );
}
