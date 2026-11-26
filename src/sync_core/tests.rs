//! Cycle-level acceptance tests: a fake iCloud zone feed, a local HTTP server
//! for file bytes, and a real state file on disk that is reopened between runs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::path as url_path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{PathDerivationCliArgs, resolve_path_derivation_fields};
use crate::icloud::photos::{PhotoLibrary, PhotosSession};

/// A fake `PrimarySync` zone: an append-only change log with integer
/// positions, current records for lookup, and switches for failure cases.
#[derive(Default)]
struct World {
    log: Vec<Value>,
    current: HashMap<String, Value>,
    page_size: usize,
    /// Positions iCloud refuses with `BAD_REQUEST`.
    rejected: HashSet<String>,
    /// Lookups that return a URL the server refuses, per record name.
    expire_next_lookups: HashMap<String, usize>,
    /// Cancel this token once the given number of pages has been served.
    cancel_after_pages: Option<(usize, CancellationToken)>,
    pages_served: usize,
    requested_positions: Vec<Option<String>>,
    looked_up: Vec<String>,
}

#[derive(Clone)]
struct FakeICloud(Arc<Mutex<World>>);

#[async_trait::async_trait]
impl PhotosSession for FakeICloud {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let body: Value = serde_json::from_str(&body)?;
        let mut world = self.0.lock().unwrap();
        if url.contains("/changes/zone") {
            let position = body["zones"][0]["syncToken"]
                .as_str()
                .map(ToOwned::to_owned);
            world.requested_positions.push(position.clone());
            if let Some(position) = &position
                && world.rejected.contains(position)
            {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": "PrimarySync"}, "syncToken": "", "moreComing": false,
                    "serverErrorCode": "BAD_REQUEST", "reason": "Unknown sync continuation type"
                }]}));
            }
            let start = position.as_deref().map_or(0, |position| {
                position.trim_start_matches('p').parse::<usize>().unwrap()
            });
            let end = (start + world.page_size).min(world.log.len());
            let records = world.log.get(start..end).unwrap_or_default().to_vec();
            world.pages_served += 1;
            if let Some((after, token)) = &world.cancel_after_pages
                && world.pages_served >= *after
            {
                token.cancel();
            }
            return Ok(json!({"zones": [{
                "zoneID": {"zoneName": "PrimarySync"},
                "syncToken": format!("p{end}"),
                "moreComing": end < world.log.len(),
                "records": records,
            }]}));
        }
        if url.contains("/records/lookup") {
            let mut records = Vec::new();
            for name in body["records"].as_array().unwrap() {
                let name = name["recordName"].as_str().unwrap().to_owned();
                world.looked_up.push(name.clone());
                match world.current.get(&name).cloned() {
                    Some(mut record) => {
                        let remaining = world.expire_next_lookups.entry(name.clone()).or_default();
                        if *remaining > 0 {
                            *remaining -= 1;
                            if let Some(res) = record.pointer_mut("/fields/resOriginalRes/value") {
                                let url = res["downloadURL"]
                                    .as_str()
                                    .unwrap()
                                    .replace("/media/", "/expired/");
                                res["downloadURL"] = json!(url);
                            }
                        }
                        records.push(record);
                    }
                    None => {
                        records.push(json!({"recordName": name, "serverErrorCode": "NOT_FOUND"}))
                    }
                }
            }
            return Ok(json!({"records": records}));
        }
        anyhow::bail!("unexpected request {url}")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

struct Harness {
    world: FakeICloud,
    server: MockServer,
    root: tempfile::TempDir,
    bytes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Harness {
    async fn new() -> Self {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::path_regex("^/expired/.*"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;
        let world = World {
            page_size: 3,
            ..World::default()
        };
        Self {
            world: FakeICloud(Arc::new(Mutex::new(world))),
            server,
            root: tempfile::tempdir().unwrap(),
            bytes: Arc::default(),
        }
    }

    fn photos(&self) -> PathBuf {
        self.root.path().join("photos")
    }

    fn library(&self) -> PhotoLibrary {
        PhotoLibrary::new_stub(Box::new(self.world.clone()))
    }

    fn config(&self) -> CoreConfig {
        let directory = self.photos();
        let fields =
            resolve_path_derivation_fields(PathDerivationCliArgs::default(), None).unwrap();
        let mut paths = PathDerivationConfig::from_path_fields(
            Arc::from(directory.as_path()),
            fields,
            crate::config::MediaSelection::default(),
        );
        paths.folder_structure = "%Y".into();
        CoreConfig {
            directory,
            paths,
            albums: AlbumSelector::default(),
            unfiled: true,
            temp_suffix: ".kei-part".into(),
            concurrency: 4,
            retry: RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            },
        }
    }

    async fn state(&self) -> State {
        State::open(&self.root.path().join("state.sync.db"))
            .await
            .unwrap()
    }

    async fn run(&self) -> CycleReport {
        self.run_with(&CancellationToken::new()).await
    }

    async fn run_with(&self, cancel: &CancellationToken) -> CycleReport {
        let state = self.state().await;
        run_cycle(
            &state,
            &[self.library()],
            &self.config(),
            &reqwest::Client::new(),
            cancel,
        )
        .await
        .unwrap()
    }

    fn push(&self, record: Value) {
        let mut world = self.world.0.lock().unwrap();
        let name = record["recordName"].as_str().unwrap().to_owned();
        if record.get("deleted") == Some(&json!(true)) {
            world.current.remove(&name);
        } else {
            world.current.insert(name, record.clone());
        }
        world.log.push(record);
    }

    /// Serve `bytes` for a master and return its record.
    async fn master(&self, name: &str, filename: &str, bytes: &[u8]) -> Value {
        let version = format!("{name}-{}", self.bytes.lock().unwrap().len());
        self.bytes
            .lock()
            .unwrap()
            .insert(version.clone(), bytes.to_vec());
        Mock::given(url_path(format!("/media/{version}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .mount(&self.server)
            .await;
        let checksum = base64::engine::general_purpose::STANDARD.encode(version.as_bytes());
        json!({
            "recordName": name, "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": filename, "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": bytes.len(), "fileChecksum": checksum,
                    "downloadURL": format!("{}/media/{version}", self.server.uri())
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }
        })
    }

    fn file(&self, relative: &str) -> Option<Vec<u8>> {
        std::fs::read(self.photos().join(relative)).ok()
    }

    fn take_positions(&self) -> Vec<Option<String>> {
        std::mem::take(&mut self.world.0.lock().unwrap().requested_positions)
    }

    fn take_lookups(&self) -> Vec<String> {
        std::mem::take(&mut self.world.0.lock().unwrap().looked_up)
    }
}

fn jpeg(tag: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.extend_from_slice(tag.as_bytes());
    bytes
}

/// 2023-11-14 UTC.
const CAPTURED_MS: f64 = 1_700_000_000_000.0;

fn asset(name: &str, master: &str) -> Value {
    json!({
        "recordName": name, "recordType": "CPLAsset",
        "fields": {
            "masterRef": {"value": {"recordName": master, "zoneID": {"zoneName": "PrimarySync"}}},
            "assetDate": {"value": CAPTURED_MS},
            "addedDate": {"value": CAPTURED_MS}
        }
    })
}

fn album(name: &str, title: &str) -> Value {
    json!({
        "recordName": name, "recordType": "CPLAlbum",
        "fields": {"albumNameEnc": {"value": title, "type": "STRING"}, "albumType": {"value": 0}}
    })
}

fn member(asset: &str, album: &str) -> Value {
    json!({
        "recordName": format!("{asset}-IN-{album}"), "recordType": "CPLContainerRelation",
        "fields": {"containerId": {"value": album}, "itemId": {"value": asset}}
    })
}

fn sorted_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}

async fn library_with_album(h: &Harness) {
    h.push(album("AL", "Trip"));
    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("one")).await;
    h.push(m1);
    h.push(asset("A1", "M1"));
    let m2 = h.master("M2", "IMG_0002.JPG", &jpeg("two")).await;
    h.push(asset("A2", "M2"));
    h.push(member("A2", "AL"));
    h.push(m2);
}

#[tokio::test]
async fn full_listing_saves_unfiled_and_album_copies_then_stays_quiet() {
    let h = Harness::new().await;
    library_with_album(&h).await;

    let first = h.run().await;
    assert!(first.complete(), "{first:?}");
    assert_eq!(first.zones[0].drain.downloaded, 2);
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_0001.JPG", "Trip/IMG_0002.JPG"]
    );
    assert_eq!(h.file("Trip/IMG_0002.JPG").unwrap(), jpeg("two"));
    assert_eq!(h.take_positions()[0], None);

    let second = h.run().await;
    assert!(second.complete());
    assert_eq!(second.zones[0].pages, 1);
    assert_eq!(second.zones[0].drain.downloaded, 0);
    assert_eq!(h.take_positions(), [Some("p6".to_owned())]);
    h.take_lookups();
    let third = h.run().await;
    assert_eq!(third.zones[0].drain.downloaded, 0);
    assert!(
        h.take_lookups().is_empty(),
        "a quiet cycle looks nothing up"
    );
}

#[tokio::test]
async fn a_new_photo_needs_only_its_own_work() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;
    h.take_lookups();

    let m3 = h.master("M3", "IMG_0003.JPG", &jpeg("three")).await;
    h.push(m3);
    h.push(asset("A3", "M3"));
    let report = h.run().await;
    assert!(report.complete());
    assert_eq!(report.zones[0].drain.downloaded, 1);
    let mut lookups = h.take_lookups();
    lookups.sort();
    assert_eq!(lookups, ["A3", "M3"]);
    assert_eq!(h.file("2023/IMG_0003.JPG").unwrap(), jpeg("three"));
}

#[tokio::test]
async fn an_interrupted_listing_resumes_from_its_last_saved_page() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    let cancel = CancellationToken::new();
    h.world.0.lock().unwrap().cancel_after_pages = Some((1, cancel.clone()));
    let first = h.run_with(&cancel).await;
    assert!(!first.zones[0].caught_up);
    assert!(sorted_files(&h.photos()).is_empty());
    h.world.0.lock().unwrap().cancel_after_pages = None;
    h.take_positions();

    let second = h.run().await;
    assert!(second.complete(), "{second:?}");
    assert_eq!(h.take_positions()[0].as_deref(), Some("p3"));
    assert_eq!(sorted_files(&h.photos()).len(), 2);
}

#[tokio::test]
async fn a_rejected_position_relists_without_downloading_again() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;
    let before = std::fs::metadata(h.photos().join("2023/IMG_0001.JPG")).unwrap();
    h.world.0.lock().unwrap().rejected.insert("p6".into());

    let report = h.run().await;
    assert!(report.zones[0].position_reset);
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 0);
    let after = std::fs::metadata(h.photos().join("2023/IMG_0001.JPG")).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(sorted_files(&h.photos()).len(), 2);
}

#[tokio::test]
async fn an_expired_url_is_refreshed_once_in_the_same_cycle() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.world
        .0
        .lock()
        .unwrap()
        .expire_next_lookups
        .insert("M1".into(), 1);
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("one"));

    let h = Harness::new().await;
    library_with_album(&h).await;
    h.world
        .0
        .lock()
        .unwrap()
        .expire_next_lookups
        .insert("M1".into(), 5);
    let report = h.run().await;
    assert_eq!(report.completeness.failed, 1);
    assert_eq!(report.completeness.done, 1);
    h.world.0.lock().unwrap().expire_next_lookups.clear();
    let retried = h.run().await;
    assert!(retried.complete(), "{retried:?}");
}

#[tokio::test]
async fn new_versions_replace_only_untouched_files() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;
    std::fs::write(h.photos().join("Trip/IMG_0002.JPG"), jpeg("edited locally")).unwrap();

    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("one v2")).await;
    h.push(m1);
    let m2 = h.master("M2", "IMG_0002.JPG", &jpeg("two v2")).await;
    h.push(m2);
    let report = h.run().await;
    assert_eq!(report.zones[0].drain.downloaded, 1);
    assert_eq!(report.zones[0].drain.modified, 1);
    assert_eq!(report.completeness.modified, 1);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("one v2"));
    assert_eq!(h.file("Trip/IMG_0002.JPG").unwrap(), jpeg("edited locally"));
    assert_eq!(sorted_files(&h.photos()).len(), 2);
}

#[tokio::test]
async fn album_paths_are_stable_and_departed_copies_stay() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;

    h.push(album("AL", "Renamed Trip"));
    let m4 = h.master("M4", "IMG_0004.JPG", &jpeg("four")).await;
    h.push(m4);
    h.push(asset("A4", "M4"));
    h.push(member("A4", "AL"));
    h.push(json!({"recordName": "A2-IN-AL", "deleted": true}));
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(
        sorted_files(&h.photos()),
        [
            "2023/IMG_0001.JPG",
            "2023/IMG_0002.JPG",
            "Trip/IMG_0002.JPG",
            "Trip/IMG_0004.JPG"
        ]
    );
    // The departed copy stays on disk but is no longer owed.
    assert_eq!(report.completeness.done, 3);
    let second = h.run().await;
    assert_eq!(second.zones[0].drain.downloaded, 0);
    assert_eq!(h.file("Trip/IMG_0002.JPG").unwrap(), jpeg("two"));
}

#[tokio::test]
async fn a_photo_waits_for_its_master_and_name_collisions_get_suffixes() {
    let h = Harness::new().await;
    h.push(asset("A1", "M1"));
    let report = h.run().await;
    assert_eq!(report.zones[0].plan.waiting, 1);
    assert!(sorted_files(&h.photos()).is_empty());

    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("one")).await;
    h.push(m1);
    let other = h
        .master("M9", "IMG_0001.JPG", &jpeg("a different photo"))
        .await;
    h.push(other);
    h.push(asset("A9", "M9"));
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    let files = sorted_files(&h.photos());
    assert_eq!(files.len(), 2);
    assert!(files.contains(&"2023/IMG_0001.JPG".to_owned()));
}

#[tokio::test]
async fn an_existing_file_with_the_same_size_is_adopted() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    std::fs::create_dir_all(h.photos().join("2023")).unwrap();
    std::fs::write(h.photos().join("2023/IMG_0001.JPG"), jpeg("one")).unwrap();
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.adopted, 1);
    assert_eq!(report.zones[0].drain.downloaded, 1);
}
