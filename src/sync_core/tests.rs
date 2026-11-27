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
    /// Change logs of other zones, such as a Shared Library.
    other_logs: HashMap<String, Vec<Value>>,
    /// Queried smart folder members, by the folder's `smartAlbum` value or index.
    smart: HashMap<String, Vec<String>>,
    smart_queries: usize,
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
    /// Added to every record lookup, as network latency.
    lookup_delay: std::time::Duration,
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
        if url.contains("/records/lookup") {
            let delay = self.0.lock().unwrap().lookup_delay;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        let mut world = self.0.lock().unwrap();
        if url.contains("/changes/zone") {
            let zone = body["zones"][0]["zoneID"]["zoneName"]
                .as_str()
                .unwrap()
                .to_owned();
            let position = body["zones"][0]["syncToken"]
                .as_str()
                .map(ToOwned::to_owned);
            // Like iCloud, return only the fields the request names.
            let keys: Option<HashSet<String>> =
                body["zones"][0]["desiredKeys"].as_array().map(|keys| {
                    keys.iter()
                        .filter_map(|k| k.as_str().map(ToOwned::to_owned))
                        .collect()
                });
            let project = |records: &[Value]| -> Vec<Value> {
                records
                    .iter()
                    .map(|record| {
                        let mut record = record.clone();
                        if let (Some(keys), Some(fields)) = (
                            &keys,
                            record.get_mut("fields").and_then(Value::as_object_mut),
                        ) {
                            fields.retain(|key, _| keys.contains(key));
                        }
                        record
                    })
                    .collect()
            };
            if zone != "PrimarySync" {
                let log = world.other_logs.get(&zone).cloned().unwrap_or_default();
                let start = position.as_deref().map_or(0, |position| {
                    position
                        .rsplit_once('@')
                        .unwrap()
                        .1
                        .parse::<usize>()
                        .unwrap()
                });
                let end = (start + world.page_size).min(log.len());
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": zone},
                    "syncToken": format!("{zone}@{end}"),
                    "moreComing": end < log.len(),
                    "records": project(log.get(start..end).unwrap_or_default()),
                }]}));
            }
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
            let records = project(world.log.get(start..end).unwrap_or_default());
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
        if url.contains("/records/query") {
            world.smart_queries += 1;
            let filters = body["query"]["filterBy"].as_array().unwrap();
            let offset = filters[0]["fieldValue"]["value"].as_u64().unwrap() as usize;
            let key = filters
                .get(2)
                .and_then(|filter| filter["fieldValue"]["value"].as_str())
                .unwrap_or_else(|| body["query"]["recordType"].as_str().unwrap())
                .to_owned();
            let members = world.smart.get(&key).cloned().unwrap_or_default();
            let records: Vec<Value> = members
                .iter()
                .skip(offset)
                .take(2)
                .map(|name| json!({"recordName": name, "recordType": "CPLAsset", "fields": {}}))
                .collect();
            return Ok(json!({"records": records}));
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
    zones: Mutex<Vec<String>>,
    /// What `CheckIndexingState` reports for every zone.
    indexing_finished: Mutex<bool>,
    /// Report a state kei does not recognize instead, as iCloud's `FAILED`.
    indexing_unavailable: Mutex<bool>,
    tweak: Mutex<Option<fn(&mut CoreConfig)>>,
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
            zones: Mutex::new(vec!["PrimarySync".to_owned()]),
            indexing_finished: Mutex::new(true),
            indexing_unavailable: Mutex::new(false),
            tweak: Mutex::new(None),
            server,
            root: tempfile::tempdir().unwrap(),
            bytes: Arc::default(),
        }
    }

    fn pages_served(&self) -> usize {
        self.world.0.lock().unwrap().pages_served
    }

    fn photos(&self) -> PathBuf {
        self.root.path().join("photos")
    }

    fn libraries(&self) -> Vec<PhotoLibrary> {
        self.zones
            .lock()
            .unwrap()
            .iter()
            .map(|zone| {
                let library = PhotoLibrary::new_stub_with_zone(Box::new(self.world.clone()), zone)
                    .with_stub_indexing(*self.indexing_finished.lock().unwrap());
                if *self.indexing_unavailable.lock().unwrap() {
                    library.with_stub_indexing_unavailable()
                } else {
                    library
                }
            })
            .collect()
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
        let mut config = CoreConfig {
            directory,
            paths,
            albums: AlbumSelector::default(),
            smart_folders: crate::selection::SmartFolderSelector::None,
            unfiled: true,
            temp_suffix: ".kei-part".into(),
            concurrency: 4,
            retry: RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            },
            transfer: Transfer::All,
            bandwidth: None,
            metadata: None,
            refresh_metadata: false,
        };
        if let Some(tweak) = *self.tweak.lock().unwrap() {
            tweak(&mut config);
        }
        config
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
            &self.libraries(),
            &self.config(),
            &reqwest::Client::new(),
            cancel,
        )
        .await
        .unwrap()
    }

    fn push(&self, record: Value) {
        self.push_to("PrimarySync", record);
    }

    fn push_to(&self, zone: &str, record: Value) {
        let mut world = self.world.0.lock().unwrap();
        let name = record["recordName"].as_str().unwrap().to_owned();
        if record.get("deleted") == Some(&json!(true)) {
            world.current.remove(&name);
        } else {
            world.current.insert(name, record.clone());
        }
        if zone == "PrimarySync" {
            world.log.push(record);
        } else {
            world
                .other_logs
                .entry(zone.to_owned())
                .or_default()
                .push(record);
        }
    }

    /// Serve `bytes` for a master and return its record.
    async fn master(&self, name: &str, filename: &str, bytes: &[u8]) -> Value {
        self.master_delayed(name, filename, bytes, std::time::Duration::ZERO)
            .await
    }

    /// Like [`Self::master`], with each download taking at least `delay`.
    async fn master_delayed(
        &self,
        name: &str,
        filename: &str,
        bytes: &[u8],
        delay: std::time::Duration,
    ) -> Value {
        let version = format!("{name}-{}", self.bytes.lock().unwrap().len());
        self.bytes
            .lock()
            .unwrap()
            .insert(version.clone(), bytes.to_vec());
        Mock::given(url_path(format!("/media/{version}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes.to_vec())
                    .set_delay(delay),
            )
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
async fn listing_carries_on_while_icloud_indexes_then_repeats_once() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    *h.indexing_finished.lock().unwrap() = false;

    let first = h.run().await;
    assert!(first.zones[0].indexing);
    assert!(first.zones[0].caught_up);
    assert!(
        !first.complete(),
        "a listing during indexing is provisional"
    );
    assert_eq!(first.zones[0].drain.downloaded, 2);
    assert_eq!(h.take_positions()[0], None);

    // Still indexing: read changes as usual.
    h.run().await;
    assert_eq!(h.take_positions(), [Some("p6".to_owned())]);

    // Indexing finished: list from the start once, interrupted part way.
    *h.indexing_finished.lock().unwrap() = true;
    let cancel = CancellationToken::new();
    h.world.0.lock().unwrap().cancel_after_pages = Some((h.pages_served() + 1, cancel.clone()));
    let interrupted = h.run_with(&cancel).await;
    assert!(interrupted.zones[0].indexing);
    assert_eq!(h.take_positions(), [None]);
    h.world.0.lock().unwrap().cancel_after_pages = None;

    // The restart continues that listing instead of starting it again.
    let resumed = h.run().await;
    assert!(resumed.complete(), "{resumed:?}");
    assert!(!resumed.zones[0].indexing);
    assert_eq!(resumed.zones[0].drain.downloaded, 0);
    assert_eq!(h.take_positions(), [Some("p3".to_owned())]);

    let quiet = h.run().await;
    assert!(quiet.complete());
    assert_eq!(h.take_positions(), [Some("p6".to_owned())]);
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_0001.JPG", "Trip/IMG_0002.JPG"]
    );
}

#[tokio::test]
async fn an_index_icloud_reports_as_failed_does_not_hold_the_library() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.smart_folders = crate::selection::SmartFolderSelector::Named {
            included: ["Screenshots".to_owned()].into(),
            excluded: Default::default(),
        };
    });
    for (asset_name, master, file) in [("A1", "M1", "PLAIN.JPG"), ("A2", "M2", "SHOT.JPG")] {
        let record = h.master(master, file, &jpeg(file)).await;
        h.push(record);
        h.push(asset(asset_name, master));
    }
    h.world
        .0
        .lock()
        .unwrap()
        .smart
        .insert("SCREENSHOT".into(), vec!["A2".into()]);
    // iCloud reports FAILED for good while the library reads normally.
    *h.indexing_unavailable.lock().unwrap() = true;

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert!(!report.zones[0].indexing);
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/PLAIN.JPG", "Screenshots/SHOT.JPG"]
    );
    h.take_positions();
    let quiet = h.run().await;
    assert!(quiet.complete(), "{quiet:?}");
    assert_eq!(h.take_positions(), [Some("p4".to_owned())], "no relist");
}

#[tokio::test]
async fn a_listing_taken_while_indexing_is_repeated_once_when_the_state_turns_failed() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    *h.indexing_finished.lock().unwrap() = false;
    let first = h.run().await;
    assert!(first.zones[0].indexing);
    h.take_positions();

    *h.indexing_unavailable.lock().unwrap() = true;
    let relisted = h.run().await;
    assert!(relisted.complete(), "{relisted:?}");
    assert!(!relisted.zones[0].indexing);
    assert_eq!(h.take_positions()[0], None, "listed again from the start");
    assert_eq!(relisted.zones[0].drain.downloaded, 0);

    let quiet = h.run().await;
    assert!(quiet.complete());
    assert_eq!(h.take_positions(), [Some("p6".to_owned())]);
}

#[tokio::test]
async fn queried_smart_folders_wait_for_indexing_before_new_photos_are_placed() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.smart_folders = crate::selection::SmartFolderSelector::Named {
            included: ["Screenshots".to_owned()].into(),
            excluded: Default::default(),
        };
    });
    for (asset_name, master, file) in [("A1", "M1", "PLAIN.JPG"), ("A2", "M2", "SHOT.JPG")] {
        let record = h.master(master, file, &jpeg(file)).await;
        h.push(record);
        h.push(asset(asset_name, master));
    }
    h.world
        .0
        .lock()
        .unwrap()
        .smart
        .insert("SCREENSHOT".into(), vec!["A2".into()]);
    *h.indexing_finished.lock().unwrap() = false;

    let waiting = h.run().await;
    assert!(waiting.zones[0].caught_up);
    assert_eq!(h.world.0.lock().unwrap().smart_queries, 0);
    assert!(
        sorted_files(&h.photos()).is_empty(),
        "a screenshot placed now would also be saved as unfiled"
    );

    *h.indexing_finished.lock().unwrap() = true;
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/PLAIN.JPG", "Screenshots/SHOT.JPG"]
    );

    // Indexing again later: the stored members are kept and used.
    *h.indexing_finished.lock().unwrap() = false;
    let m3 = h.master("M3", "NEW.JPG", &jpeg("NEW.JPG")).await;
    h.push(m3);
    h.push(asset("A3", "M3"));
    let report = h.run().await;
    assert_eq!(report.zones[0].drain.downloaded, 1);
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/NEW.JPG", "2023/PLAIN.JPG", "Screenshots/SHOT.JPG"]
    );
}

#[tokio::test]
async fn a_malformed_record_does_not_hold_up_the_library() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.push(json!({"recordName": "BROKEN", "recordType": "CPLContainerRelation", "fields": {}}));

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].skipped_records, 1);
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_0001.JPG", "Trip/IMG_0002.JPG"]
    );
    let quiet = h.run().await;
    assert_eq!(quiet.zones[0].skipped_records, 0);
}

#[test]
fn health_counts_new_failures_without_progress_not_repeated_ones() {
    let zone = |error: Option<&str>, drain: drain::DrainStats| ZoneReport {
        error: error.map(ToOwned::to_owned),
        drain,
        ..ZoneReport::default()
    };
    let report = |zones| CycleReport {
        zones,
        ..CycleReport::default()
    };
    let repeated = drain::DrainStats {
        failed: 1,
        ..Default::default()
    };
    assert_eq!(report(vec![zone(None, repeated)]).health_problem(), None);
    let new_with_progress = drain::DrainStats {
        failed: 2,
        newly_failed: 2,
        downloaded: 1,
        ..Default::default()
    };
    assert_eq!(
        report(vec![zone(None, new_with_progress)]).health_problem(),
        None
    );
    let stuck = drain::DrainStats {
        failed: 3,
        newly_failed: 3,
        ..Default::default()
    };
    assert!(report(vec![zone(None, stuck)]).health_problem().is_some());
    assert_eq!(
        report(vec![zone(Some("iCloud refused"), repeated)])
            .health_problem()
            .as_deref(),
        Some("iCloud refused")
    );
}

/// The asset record with an adjusted rendition served as `bytes`, as iCloud
/// reports a photo edited on a device.
async fn edited_asset(h: &Harness, name: &str, master: &str, bytes: &[u8]) -> Value {
    let served = h.master(&format!("{name}-edit"), "unused.JPG", bytes).await;
    with_fields(
        asset(name, master),
        json!({
            "resJPEGFullRes": served["fields"]["resOriginalRes"].clone(),
            "resJPEGFullFileType": {"value": "public.jpeg"},
            "adjustmentType": {"value": "com.apple.photo"}
        }),
    )
}

#[tokio::test]
async fn primary_naming_hands_the_plain_name_to_the_edit_and_back() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.paths.edited_naming = crate::types::EditedNaming::Primary;
    });
    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("original")).await;
    h.push(m1);
    h.push(asset("A1", "M1"));
    h.run().await;
    assert_eq!(sorted_files(&h.photos()), ["2023/IMG_0001.JPG"]);

    // Edited on a device: the plain name takes the edit and the original
    // is kept beside it.
    let edited = edited_asset(&h, "A1", "M1", &jpeg("edit one")).await;
    h.push(edited);
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_0001.JPG", "2023/IMG_0001_original.JPG"]
    );
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("edit one"));
    assert_eq!(
        h.file("2023/IMG_0001_original.JPG").unwrap(),
        jpeg("original")
    );
    // Scanners see the plain name change; the original keeps its capture time.
    let mtime = |relative: &str| {
        std::fs::metadata(h.photos().join(relative))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    };
    assert!(mtime("2023/IMG_0001.JPG") > CAPTURED_MS / 1000.0 + 86_400.0);
    assert!((mtime("2023/IMG_0001_original.JPG") - CAPTURED_MS / 1000.0).abs() < 2.0);

    // Edited again: only the plain name changes.
    let edited = edited_asset(&h, "A1", "M1", &jpeg("edit two")).await;
    h.push(edited);
    let report = h.run().await;
    assert_eq!(report.zones[0].drain.downloaded, 1);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("edit two"));

    // Reverted: the plain name holds the original again, and the copy
    // beside it stays.
    h.push(asset("A1", "M1"));
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 1);
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_0001.JPG", "2023/IMG_0001_original.JPG"]
    );
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("original"));
    let quiet = h.run().await;
    assert_eq!(quiet.zones[0].drain, drain::DrainStats::default());
    assert_eq!(sorted_files(&h.photos()).len(), 2);

    // Edited again after a local change to the plain name: the local file
    // is kept and reported, and the original beside it is reused.
    std::fs::write(h.photos().join("2023/IMG_0001.JPG"), jpeg("changed here")).unwrap();
    let edited = edited_asset(&h, "A1", "M1", &jpeg("edit three")).await;
    h.push(edited);
    let report = h.run().await;
    assert_eq!(report.completeness.modified, 1);
    assert_eq!(report.zones[0].drain.downloaded, 0);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("changed here"));
    assert_eq!(
        h.file("2023/IMG_0001_original.JPG").unwrap(),
        jpeg("original")
    );
}

#[tokio::test]
async fn a_new_download_folder_gets_every_copy_not_moved_along() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;
    // Move one copy along by hand; the other stays behind.
    let moved = h.root.path().join("moved");
    std::fs::create_dir_all(moved.join("2023")).unwrap();
    std::fs::rename(
        h.photos().join("2023/IMG_0001.JPG"),
        moved.join("2023/IMG_0001.JPG"),
    )
    .unwrap();
    *h.tweak.lock().unwrap() = Some(|config| {
        let moved = config.directory.with_file_name("moved");
        config.directory = moved.clone();
        config.paths.directory = std::sync::Arc::from(moved.as_path());
    });

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 1);
    assert_eq!(
        sorted_files(&moved),
        ["2023/IMG_0001.JPG", "Trip/IMG_0002.JPG"]
    );
    assert_eq!(
        sorted_files(&h.photos()),
        ["Trip/IMG_0002.JPG"],
        "the old folder is left as it was"
    );
    let quiet = h.run().await;
    assert_eq!(quiet.zones[0].drain.downloaded, 0);
}

/// QuickTime bytes: an `ftyp` atom, then `tag`.
fn mov(tag: &str) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0x14];
    bytes.extend_from_slice(b"ftypqt  \0\0\0\0qt  ");
    bytes.extend_from_slice(tag.as_bytes());
    bytes
}

/// A resource field pointing at a fresh server URL for `bytes`.
async fn served_resource(h: &Harness, tag: &str, bytes: &[u8]) -> Value {
    h.master(tag, "unused.JPG", bytes).await["fields"]["resOriginalRes"].clone()
}

/// An unedited Live Photo: master `master` with still and motion.
async fn live_photo_named(h: &Harness, master: &str, name: &str, filename: &str) {
    let mut record = h
        .master(master, filename, &jpeg(&format!("{master} still")))
        .await;
    record["fields"]["resOriginalVidComplRes"] = served_resource(
        h,
        &format!("{master}-mov"),
        &mov(&format!("{master} motion")),
    )
    .await;
    record["fields"]["resOriginalVidComplFileType"] = json!({"value": "com.apple.quicktime-movie"});
    h.push(record);
    h.push(asset(name, master));
}

/// A Live Photo in album `AL`: master `M1` with still and motion; the asset
/// optionally carries an edit of both.
async fn live_photo(h: &Harness, edit: Option<(&[u8], &[u8])>) {
    let mut master = h.master("M1", "IMG_0001.JPG", &jpeg("still")).await;
    master["fields"]["resOriginalVidComplRes"] = served_resource(h, "M1-mov", &mov("motion")).await;
    master["fields"]["resOriginalVidComplFileType"] = json!({"value": "com.apple.quicktime-movie"});
    h.push(master);
    let mut record = asset("A1", "M1");
    if let Some((still, motion)) = edit {
        record["fields"]["resJPEGFullRes"] = served_resource(h, "A1-still", still).await;
        record["fields"]["resJPEGFullFileType"] = json!({"value": "public.jpeg"});
        record["fields"]["resVidComplRes"] = served_resource(h, "A1-mov", motion).await;
        record["fields"]["resVidComplFileType"] = json!({"value": "com.apple.quicktime-movie"});
        record["fields"]["adjustmentType"] = json!({"value": "com.apple.photo"});
    }
    h.push(record);
}

fn contents(h: &Harness) -> Vec<(String, String)> {
    sorted_files(&h.photos())
        .into_iter()
        .map(|f| {
            let bytes = h.file(&f).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            let tag = text.rsplit(['\u{FFFD}', ' ']).next().unwrap_or_default();
            (f, tag.to_owned())
        })
        .collect()
}

#[tokio::test]
async fn an_interrupted_handover_of_the_plain_name_is_recorded_as_kei_s_own() {
    fn primary(config: &mut CoreConfig) {
        config.paths.edited_naming = crate::types::EditedNaming::Primary;
    }
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(primary);
    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("original")).await;
    h.push(m1);
    h.push(asset("A1", "M1"));
    h.run().await;

    // Plan the edit without downloading it.
    let edit = jpeg("edit");
    let edited = edited_asset(&h, "A1", "M1", &edit).await;
    h.push(edited);
    *h.tweak.lock().unwrap() = Some(|config| {
        primary(config);
        config.transfer = Transfer::None;
    });
    h.run().await;
    // kei wrote its receipt, published the edit, then stopped.
    let state = h.state().await;
    let row = state.file_at("2023/IMG_0001.JPG").await.unwrap().unwrap();
    assert_eq!(row.rendition, "adjusted");
    state
        .set_pending(row.id, &sha256_hex(&edit), None)
        .await
        .unwrap();
    drop(state);
    std::fs::write(h.photos().join("2023/IMG_0001.JPG"), &edit).unwrap();

    *h.tweak.lock().unwrap() = Some(primary);
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.completeness.modified, 0);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), edit);
    assert_eq!(
        h.file("2023/IMG_0001_original.JPG").unwrap(),
        jpeg("original")
    );
    let row = h
        .state()
        .await
        .file_at("2023/IMG_0001.JPG")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.sha256, Some(sha256_hex(&edit)));
    let quiet = h.run().await;
    assert_eq!(quiet.zones[0].drain, drain::DrainStats::default());
}

#[tokio::test]
async fn primary_naming_moves_a_live_photos_still_and_motion_together_in_an_album() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.paths.edited_naming = crate::types::EditedNaming::Primary;
    });
    h.push(album("AL", "Trip"));
    live_photo(&h, None).await;
    h.push(member("A1", "AL"));
    h.run().await;
    let pair = |still: &str, motion: &str| {
        [
            ("Trip/IMG_0001.JPG", still),
            ("Trip/IMG_0001.MOV", motion),
            ("Trip/IMG_0001_original.JPG", "still"),
            ("Trip/IMG_0001_original.MOV", "motion"),
        ]
        .map(|(path, tag)| (path.to_owned(), tag.to_owned()))
        .to_vec()
    };
    assert_eq!(
        contents(&h),
        pair("still", "motion")[..2].to_vec(),
        "no edit: plain names hold the originals"
    );

    live_photo(&h, Some((&jpeg("stilledit"), &mov("motionedit")))).await;
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(contents(&h), pair("stilledit", "motionedit"));

    live_photo(&h, None).await;
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 2);
    assert_eq!(contents(&h), pair("still", "motion"));

    let quiet = h.run().await;
    assert!(quiet.complete());
    assert_eq!(quiet.zones[0].drain, drain::DrainStats::default());
    assert_eq!(contents(&h), pair("still", "motion"));
}

/// A tiny valid JPEG whose pixel density encodes `n`, so every photo's
/// bytes differ.
#[cfg(feature = "xmp")]
fn tiny_jpeg(n: u16) -> Vec<u8> {
    let [hi, lo] = n.to_be_bytes();
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, hi, lo,
        0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ]
}

/// Every file under `root` with its bytes and modification time.
fn snapshot(root: &Path) -> std::collections::BTreeMap<String, (Vec<u8>, std::time::SystemTime)> {
    sorted_files(root)
        .into_iter()
        .map(|file| {
            let path = root.join(&file);
            let bytes = std::fs::read(&path).unwrap();
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            (file, (bytes, modified))
        })
        .collect()
}

/// Run by `scripts/test-copies-stress.sh`. 300 photos, each in two albums,
/// give 600 copies with XMP sidecars across six cycles that each reopen the
/// state. Unrelated feed records, a settings change that plans every photo
/// again, and one photo joining an unselected album must download nothing
/// and rewrite only that photo's two sidecars; every other file keeps its
/// bytes and modification time.
#[cfg(feature = "xmp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "copies stress; run by scripts/test-copies-stress.sh"]
async fn copies_stress_600_album_copies_with_sidecars_stay_quiet_across_reopens() {
    use crate::download::metadata_pass::{CaptureTimestampRepair, MetadataFlags, Settings};
    const PHOTOS: u16 = 300;
    const COPIES: u64 = 2 * PHOTOS as u64;

    fn selection(config: &mut CoreConfig) {
        config.albums = AlbumSelector::Named {
            included: ["Album A".to_owned(), "Album B".to_owned()].into(),
            excluded: Default::default(),
        };
        config.metadata = Some(Settings {
            flags: MetadataFlags::XMP_SIDECAR | MetadataFlags::INCLUDE_ALBUMS,
            repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".kei-part".into(),
        });
    }
    fn replanned(config: &mut CoreConfig) {
        selection(config);
        // Changes the planning fingerprint; no copy's path depends on it.
        config.unfiled = false;
    }

    let h = Harness::new().await;
    h.world.0.lock().unwrap().page_size = 200;
    *h.tweak.lock().unwrap() = Some(selection);
    h.push(album("ALA", "Album A"));
    h.push(album("ALB", "Album B"));
    for n in 0..PHOTOS {
        let (asset_name, master) = (format!("A{n:04}"), format!("M{n:04}"));
        let record = h
            .master(&master, &format!("IMG_{n:04}.JPG"), &tiny_jpeg(n + 1))
            .await;
        h.push(record);
        h.push(asset(&asset_name, &master));
        h.push(member(&asset_name, "ALA"));
        h.push(member(&asset_name, "ALB"));
    }
    let media_requests = async || {
        h.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().starts_with("/media/"))
            .count()
    };

    // Cycle 1: every copy is downloaded once and gets its sidecar.
    let first = h.run().await;
    assert!(first.complete(), "{first:?}");
    assert_eq!(first.zones[0].drain.downloaded, COPIES);
    assert_eq!(first.zones[0].metadata.written, COPIES, "{first:?}");
    let saved = snapshot(&h.photos());
    assert_eq!(saved.len(), 2 * COPIES as usize);
    for n in 0..PHOTOS {
        for album_folder in ["Album A", "Album B"] {
            let media = format!("{album_folder}/IMG_{n:04}.JPG");
            assert_eq!(saved[&media].0, tiny_jpeg(n + 1), "{media}");
            let xmp = String::from_utf8(saved[&format!("{media}.xmp")].0.clone()).unwrap();
            assert!(
                xmp.contains("Album A") && xmp.contains("Album B"),
                "{media}: {xmp}"
            );
        }
    }
    let downloads = media_requests().await;
    h.take_lookups();

    let quiet = |report: &CycleReport, cycle: &str| {
        assert!(report.complete(), "{cycle}: {report:?}");
        assert_eq!(
            report.zones[0].drain,
            drain::DrainStats::default(),
            "{cycle}"
        );
        assert_eq!(report.zones[0].metadata.written, 0, "{cycle}");
    };

    // Cycle 2: unrelated records arrive.
    h.push(json!({"recordName": "OPAQUE-1", "recordType": "FutureOpaque", "fields": {}}));
    h.push(album("ALC", "Album C"));
    let report = h.run().await;
    quiet(&report, "unrelated records");
    assert!(h.take_lookups().is_empty());
    assert_eq!(media_requests().await, downloads);
    assert_eq!(
        snapshot(&h.photos()),
        saved,
        "unrelated records rewrote files"
    );

    // Cycle 3: a settings change plans every photo again.
    *h.tweak.lock().unwrap() = Some(replanned);
    let report = h.run().await;
    quiet(&report, "replanned");
    assert_eq!(report.zones[0].plan.planned, u64::from(PHOTOS));
    assert_eq!(report.zones[0].metadata.unchanged, COPIES, "{report:?}");
    assert!(h.take_lookups().is_empty());
    assert_eq!(media_requests().await, downloads);
    assert_eq!(snapshot(&h.photos()), saved, "planning again rewrote files");

    // Cycle 4: one photo joins an unselected album. Only its sidecars change.
    h.push(member("A0000", "ALC"));
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain, drain::DrainStats::default());
    assert_eq!(report.zones[0].metadata.written, 2, "{report:?}");
    assert_eq!(media_requests().await, downloads);
    let changed = snapshot(&h.photos());
    let differing: Vec<&String> = saved
        .keys()
        .filter(|file| saved[*file] != changed[*file])
        .collect();
    assert_eq!(
        differing,
        ["Album A/IMG_0000.JPG.xmp", "Album B/IMG_0000.JPG.xmp"]
    );
    assert!(String::from_utf8_lossy(&changed["Album A/IMG_0000.JPG.xmp"].0).contains("Album C"));

    // Cycles 5 and 6: nothing changes, nothing is touched.
    for cycle in ["quiet", "quiet again"] {
        let report = h.run().await;
        quiet(&report, cycle);
        assert_eq!(report.zones[0].plan.planned, 0, "{cycle}");
        assert!(h.take_lookups().is_empty(), "{cycle}");
        assert_eq!(media_requests().await, downloads, "{cycle}");
        assert_eq!(snapshot(&h.photos()), changed, "{cycle}");
    }
}

/// Throughput under network latency. Prints the time to list and plan, and
/// files per second for downloads: 400 photos, each download taking 20 ms
/// and one in fifty taking 1.5 s like a large video, with 150 ms lookups.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "throughput benchmark; run with --ignored --nocapture"]
async fn bench_throughput_under_latency() {
    use std::time::{Duration, Instant};
    const PHOTOS: usize = 400;
    let h = Harness::new().await;
    {
        let mut world = h.world.0.lock().unwrap();
        world.page_size = 500;
        world.lookup_delay = Duration::from_millis(150);
    }
    for index in 0..PHOTOS {
        let delay = if index % 50 == 7 {
            Duration::from_millis(1500)
        } else {
            Duration::from_millis(20)
        };
        let master = format!("M{index:04}");
        let record = h
            .master_delayed(
                &master,
                &format!("IMG_{index:04}.JPG"),
                &jpeg(&master),
                delay,
            )
            .await;
        h.push(record);
        h.push(asset(&format!("A{index:04}"), &master));
    }

    *h.tweak.lock().unwrap() = Some(|config| {
        config.concurrency = 10;
        config.transfer = Transfer::None;
    });
    let start = Instant::now();
    h.run().await;
    let plan = start.elapsed();

    *h.tweak.lock().unwrap() = Some(|config| config.concurrency = 10);
    let start = Instant::now();
    let report = h.run().await;
    let drain = start.elapsed();
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, PHOTOS as u64);
    #[expect(clippy::cast_precision_loss, reason = "a small count")]
    let rate = PHOTOS as f64 / drain.as_secs_f64();
    eprintln!(
        "BENCH list+plan={:.2}s downloads={:.2}s files_per_sec={rate:.1} files_per_hour={:.0}",
        plan.as_secs_f64(),
        drain.as_secs_f64(),
        rate * 3600.0
    );
}

#[tokio::test]
async fn photos_with_one_name_planned_together_get_distinct_paths() {
    let h = Harness::new().await;
    // Three photos named alike on one day, planned in one batch; two have
    // the same size, so the size suffix collides too.
    for (asset_name, master, bytes) in [
        ("A1", "M1", jpeg("one")),
        ("A2", "M2", jpeg("two")),
        ("A3", "M3", jpeg("three")),
    ] {
        let record = h.master(master, "IMG_0001.JPG", &bytes).await;
        h.push(record);
        h.push(asset(asset_name, master));
    }
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 3);
    let files = sorted_files(&h.photos());
    assert_eq!(files.len(), 3, "{files:?}");
    let mut contents: Vec<Vec<u8>> = files.iter().map(|f| h.file(f).unwrap()).collect();
    contents.sort();
    let mut expected = vec![jpeg("one"), jpeg("two"), jpeg("three")];
    expected.sort();
    assert_eq!(contents, expected);
    let quiet = h.run().await;
    assert_eq!(quiet.zones[0].drain, drain::DrainStats::default());
    assert_eq!(sorted_files(&h.photos()), files);
}

#[tokio::test]
#[tracing_test::traced_test]
async fn a_photo_in_an_album_without_a_name_waits_instead_of_vanishing() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.albums = AlbumSelector::All {
            excluded: Default::default(),
        };
    });
    h.push(json!({
        "recordName": "AL", "recordType": "CPLAlbum",
        "fields": {"albumType": {"value": 0}}
    }));
    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("one")).await;
    h.push(m1);
    h.push(asset("A1", "M1"));
    h.push(member("A1", "AL"));

    let report = h.run().await;
    assert_eq!(report.zones[0].plan.waiting, 1, "{report:?}");
    assert!(sorted_files(&h.photos()).is_empty());
    assert!(logs_contain(
        "Some albums arrived without a name; their photos wait until iCloud reports it"
    ));

    // The name arrives: the photo is saved in its album.
    h.push(album("AL", "Trip"));
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(sorted_files(&h.photos()), ["Trip/IMG_0001.JPG"]);
}

#[tokio::test]
async fn a_catalog_saved_without_album_names_is_listed_again_once() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_0001.JPG", "Trip/IMG_0002.JPG"]
    );
    // A catalog from a build that did not ask for album fields.
    let db = rusqlite::Connection::open(h.root.path().join("state.sync.db")).unwrap();
    db.execute("UPDATE albums SET title = NULL, folder = NULL", [])
        .unwrap();
    drop(db);
    h.take_positions();

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(h.take_positions()[0], None, "listed from the start");
    assert_eq!(report.zones[0].drain.downloaded, 0);
    assert_eq!(
        h.state().await.unnamed_albums("PrimarySync").await.unwrap(),
        0
    );

    // Once is enough.
    let quiet = h.run().await;
    assert_eq!(h.take_positions(), [Some("p6".to_owned())]);
    assert_eq!(quiet.zones[0].drain, drain::DrainStats::default());
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
async fn contract_replace_requires_recorded_bytes_new_versions_replace_only_untouched_files() {
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
async fn contract_sync_never_deletes_album_paths_are_stable_and_departed_copies_stay() {
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
async fn contract_unknown_provider_identity_remains_pending_until_its_master_arrives() {
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

const SHARED: &str = "SharedSync-ABCDEF12-0000";

fn stand_in(name: &str, target: &str) -> Value {
    json!({
        "recordName": name, "recordType": "CPLAsset",
        "fields": {
            "assetDate": {"value": CAPTURED_MS},
            "isSparsePrivateRecord": {"value": 1, "type": "INT64"},
            "linkedShareRecordName": {"value": target, "type": "STRING"},
            "linkedShareZoneName": {"value": SHARED, "type": "STRING"},
            "linkedShareZoneOwner": {"value": {"recordName": "_owner"}, "type": "REFERENCE"}
        }
    })
}

fn with_fields(mut record: Value, fields: Value) -> Value {
    for (key, value) in fields.as_object().unwrap() {
        record["fields"][key] = value.clone();
    }
    record
}

#[tokio::test]
async fn shared_library_photos_follow_their_personal_library_albums() {
    let h = Harness::new().await;
    h.zones.lock().unwrap().push(SHARED.into());
    h.push(album("AL", "Trip"));
    h.push(stand_in("P1", "S1"));
    h.push(member("P1", "AL"));
    let s1 = h.master("MS1", "IMG_S1.JPG", &jpeg("shared one")).await;
    h.push_to(SHARED, s1);
    h.push_to(SHARED, asset("S1", "MS1"));
    let s2 = h.master("MS2", "IMG_S2.JPG", &jpeg("shared two")).await;
    h.push_to(SHARED, s2);
    h.push_to(SHARED, asset("S2", "MS2"));

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_S2.JPG", "Trip/IMG_S1.JPG"]
    );
    assert_eq!(
        report
            .zones
            .iter()
            .map(|zone| zone.plan.waiting)
            .sum::<u64>(),
        0
    );

    // The stand-in leaves the album: its shared photo becomes unfiled and
    // the album copy stays.
    h.push(json!({"recordName": "P1-IN-AL", "deleted": true}));
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(
        sorted_files(&h.photos()),
        ["2023/IMG_S1.JPG", "2023/IMG_S2.JPG", "Trip/IMG_S1.JPG"]
    );
}

#[tokio::test]
async fn smart_folders_take_their_photos_out_of_unfiled() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.smart_folders = crate::selection::SmartFolderSelector::Named {
            included: [
                "Favorites".to_owned(),
                "Screenshots".to_owned(),
                "Hidden".to_owned(),
            ]
            .into(),
            excluded: Default::default(),
        };
    });
    for (asset_name, master, file) in [
        ("A1", "M1", "FAV.JPG"),
        ("A2", "M2", "SHOT.JPG"),
        ("A3", "M3", "HIDDEN.JPG"),
        ("A4", "M4", "PLAIN.JPG"),
    ] {
        let record = h.master(master, file, &jpeg(file)).await;
        h.push(record);
        let record = match asset_name {
            "A1" => with_fields(asset("A1", "M1"), json!({"isFavorite": {"value": 1}})),
            "A3" => with_fields(asset("A3", "M3"), json!({"isHidden": {"value": 1}})),
            _ => asset(asset_name, master),
        };
        h.push(record);
    }
    h.world
        .0
        .lock()
        .unwrap()
        .smart
        .insert("SCREENSHOT".into(), vec!["A2".into()]);

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(
        sorted_files(&h.photos()),
        [
            "2023/PLAIN.JPG",
            "Favorites/FAV.JPG",
            "Hidden/HIDDEN.JPG",
            "Screenshots/SHOT.JPG"
        ]
    );

    let report = h.run().await;
    assert_eq!(report.zones[0].drain.downloaded, 0);
    assert!(report.complete());

    // Leaving a queried folder makes the photo unfiled; its old copy stays.
    h.world
        .0
        .lock()
        .unwrap()
        .smart
        .insert("SCREENSHOT".into(), Vec::new());
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert!(sorted_files(&h.photos()).contains(&"2023/SHOT.JPG".to_owned()));
    assert!(sorted_files(&h.photos()).contains(&"Screenshots/SHOT.JPG".to_owned()));
}

#[tokio::test]
async fn the_date_range_limits_which_photos_are_saved() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.paths.skip_created_before = Some(crate::config::CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
        ));
    });
    let old = h.master("M1", "OLD.JPG", &jpeg("old")).await;
    h.push(old);
    h.push(asset("A1", "M1"));
    let new = h.master("M2", "NEW.JPG", &jpeg("new")).await;
    h.push(new);
    h.push(with_fields(
        asset("A2", "M2"),
        // 2025-06-15 12:00 UTC: 2025 in every time zone.
        json!({"assetDate": {"value": 1_749_988_800_000.0_f64}}),
    ));

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(sorted_files(&h.photos()), ["2025/NEW.JPG"]);

    // Widening the range plans the earlier photo without a new listing.
    *h.tweak.lock().unwrap() = None;
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].pages, 1);
    assert_eq!(sorted_files(&h.photos()), ["2023/OLD.JPG", "2025/NEW.JPG"]);
}

#[tokio::test]
async fn stand_ins_for_an_unselected_shared_library_are_counted() {
    let h = Harness::new().await;
    h.push(stand_in("P1", "S1"));
    h.push(stand_in("P2", "S2"));
    let report = h.run().await;
    assert_eq!(report.unselected_shared, 2);
    assert!(sorted_files(&h.photos()).is_empty());

    h.zones.lock().unwrap().push(SHARED.into());
    let report = h.run().await;
    assert_eq!(report.unselected_shared, 0);
}

fn account(user: &str) -> crate::state::db::account::AccountOwner {
    crate::state::db::account::AccountOwner::authenticated(
        user,
        "com",
        &serde_json::from_value(json!({"dsInfo": {"dsid": format!("dsid-{user}")}})).unwrap(),
    )
    .unwrap()
}

/// An owned previous-engine database recording `copies` as downloaded:
/// `(master, absolute path, size, sha256, iCloud checksum)`.
async fn legacy_state(
    path: &Path,
    owner: &crate::state::db::account::AccountOwner,
    copies: &[(&str, PathBuf, u64, String, String)],
) {
    crate::test_helpers::legacy_state_db(path, owner);
    let conn = rusqlite::Connection::open(path).unwrap();
    for (master, local, size, sha256, checksum) in copies {
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes,
                 media_type, status, local_path, last_seen_at, local_checksum)
             VALUES ('PrimarySync', ?1, 'original', ?2, 'x.jpg', 0, ?3, 'photo', 'downloaded', ?4, 0, ?5)",
            rusqlite::params![master, checksum, *size as i64, local.to_string_lossy(), sha256],
        )
        .unwrap();
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn checksum_of(record: &Value) -> String {
    record["fields"]["resOriginalRes"]["value"]["fileChecksum"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn contract_upgrade_cleanup_requires_validated_state_reuses_copies_then_removes_old_state() {
    let h = Harness::new().await;
    h.push(album("AL", "Trip"));
    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("one")).await;
    let m2 = h.master("M2", "IMG_0002.JPG", &jpeg("two")).await;
    let (c1, c2) = (checksum_of(&m1), checksum_of(&m2));
    h.push(m1);
    h.push(asset("A1", "M1"));
    h.push(m2);
    h.push(asset("A2", "M2"));
    h.push(member("A2", "AL"));

    let photos = h.photos();
    std::fs::create_dir_all(photos.join("2023")).unwrap();
    std::fs::create_dir_all(photos.join("Trip")).unwrap();
    std::fs::write(photos.join("2023/IMG_0001.JPG"), jpeg("one")).unwrap();
    // The previous engine named this copy differently, in the same folder.
    std::fs::write(photos.join("Trip/IMG_0002-old.JPG"), jpeg("two")).unwrap();

    let owner = account("upgrade@example.invalid");
    let legacy = h.root.path().join("account.db");
    let size = jpeg("one").len() as u64;
    legacy_state(
        &legacy,
        &owner,
        &[
            (
                "M1",
                photos.join("2023/IMG_0001.JPG"),
                size,
                sha256_hex(&jpeg("one")),
                c1,
            ),
            (
                "M2",
                photos.join("Trip/IMG_0002-old.JPG"),
                size,
                sha256_hex(&jpeg("two")),
                c2,
            ),
            (
                "M9",
                photos.join("2023/GONE.JPG"),
                size,
                sha256_hex(b"gone"),
                "AQID".into(),
            ),
        ],
    )
    .await;
    let ours = h.root.path().join(".kei-schema-backup-ours");
    let theirs = h.root.path().join(".kei-schema-backup-theirs");
    for (bundle, database) in [
        (&ours, legacy.clone()),
        (&theirs, h.root.path().join("other.db")),
    ] {
        std::fs::create_dir_all(bundle).unwrap();
        std::fs::write(bundle.join("state.db"), b"snapshot").unwrap();
        std::fs::write(
            bundle.join("manifest.json"),
            json!({"database_path_display": std::path::absolute(&database).unwrap().display().to_string()})
                .to_string(),
        )
        .unwrap();
    }
    let legacy_before = std::fs::read(&legacy).unwrap();

    let target = State::path_for(&legacy);
    let import = upgrade::prepare(&legacy, &target, &owner, &photos)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(import.recorded, 3);
    assert_eq!(import.imported, 3);
    assert_eq!(import.changed_on_disk, 1);
    assert_eq!(std::fs::read(&legacy).unwrap(), legacy_before);
    assert!(
        upgrade::prepare(&legacy, &target, &owner, &photos)
            .await
            .unwrap()
            .is_none()
    );

    let state = State::open(&target).await.unwrap();
    let report = run_cycle(
        &state,
        &h.libraries(),
        &h.config(),
        &reqwest::Client::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 0);
    assert_eq!(report.completeness.done, 2);
    assert_eq!(
        sorted_files(&photos),
        ["2023/IMG_0001.JPG", "Trip/IMG_0002-old.JPG"]
    );

    let freed = upgrade::finish(&state).await.unwrap().unwrap();
    assert!(freed >= legacy_before.len() as u64);
    assert!(!legacy.exists());
    assert!(!ours.exists());
    assert!(theirs.join("state.db").exists());
    assert!(upgrade::finish(&state).await.unwrap().is_none());
}

#[tokio::test]
async fn an_upgrade_skips_rows_it_cannot_read_and_keeps_the_rest() {
    let root = tempfile::tempdir().unwrap();
    let photos = root.path().join("photos");
    std::fs::create_dir_all(&photos).unwrap();
    std::fs::write(photos.join("GOOD.JPG"), jpeg("good")).unwrap();
    let owner = account("upgrade@example.invalid");
    let legacy = root.path().join("account.db");
    legacy_state(
        &legacy,
        &owner,
        &[(
            "M1",
            photos.join("GOOD.JPG"),
            jpeg("good").len() as u64,
            sha256_hex(&jpeg("good")),
            "AQID".into(),
        )],
    )
    .await;
    // Values the old schema's column types allowed but this reader cannot
    // use: a size with a fraction, and a negative size.
    let conn = rusqlite::Connection::open(&legacy).unwrap();
    for (master, size) in [("M2", "3.5"), ("M3", "-1")] {
        conn.execute(
            &format!(
                "INSERT INTO assets (library, id, version_size, checksum, filename, created_at,
                     size_bytes, media_type, status, local_path, last_seen_at)
                 VALUES ('PrimarySync', ?1, 'original', 'AQID', 'x.jpg', 0.5, {size}, 'photo',
                     'downloaded', ?2, 0)"
            ),
            rusqlite::params![
                master,
                photos.join(format!("{master}.JPG")).to_string_lossy()
            ],
        )
        .unwrap();
    }
    drop(conn);

    let import = upgrade::prepare(&legacy, &State::path_for(&legacy), &owner, &photos)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(import.recorded, 3);
    assert_eq!(import.unreadable, 2);
    assert_eq!(import.imported, 1);
}

#[tokio::test]
async fn a_path_the_old_state_gave_two_photos_goes_to_the_one_whose_bytes_it_holds() {
    let h = Harness::new().await;
    // Two photos with the same name and size; the file on disk is B's.
    let (a, b) = (jpeg("aaaa"), jpeg("bbbb"));
    let m1 = h.master("M1", "IMG_0001.JPG", &a).await;
    let m2 = h.master("M2", "IMG_0001.JPG", &b).await;
    let (c1, c2) = (checksum_of(&m1), checksum_of(&m2));
    h.push(m1);
    h.push(asset("A1", "M1"));
    h.push(m2);
    h.push(asset("A2", "M2"));
    let photos = h.photos();
    std::fs::create_dir_all(photos.join("2023")).unwrap();
    std::fs::write(photos.join("2023/IMG_0001.JPG"), &b).unwrap();

    // A stale record says the file is A's; another says it is B's.
    let owner = account("upgrade@example.invalid");
    let legacy = h.root.path().join("account.db");
    let shared = photos.join("2023/IMG_0001.JPG");
    legacy_state(
        &legacy,
        &owner,
        &[
            ("M1", shared.clone(), a.len() as u64, sha256_hex(&a), c1),
            ("M2", shared, b.len() as u64, sha256_hex(&b), c2),
        ],
    )
    .await;
    let target = State::path_for(&legacy);
    let import = upgrade::prepare(&legacy, &target, &owner, &photos)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((import.imported, import.ambiguous), (1, 1));

    let state = State::open(&target).await.unwrap();
    let run = async || {
        run_cycle(
            &state,
            &h.libraries(),
            &h.config(),
            &reqwest::Client::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    };
    let report = run().await;
    assert!(report.complete(), "{report:?}");
    // B keeps its file; A is downloaded beside it instead of being
    // credited with B's bytes.
    assert_eq!(report.zones[0].drain.downloaded, 1);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), b);
    let files = sorted_files(&photos);
    assert_eq!(files.len(), 2, "{files:?}");
    let other = files.iter().find(|f| *f != "2023/IMG_0001.JPG").unwrap();
    assert_eq!(h.file(other).unwrap(), a);

    let quiet = run().await;
    assert!(quiet.complete());
    assert_eq!(quiet.zones[0].drain.downloaded, 0);
    assert_eq!(sorted_files(&photos).len(), 2);
}

#[tokio::test]
async fn an_upgrade_keeps_a_previous_primary_layout_and_follows_later_reverts() {
    let h = Harness::new().await;
    *h.tweak.lock().unwrap() = Some(|config| {
        config.paths.edited_naming = crate::types::EditedNaming::Primary;
    });
    let (original, edit) = (jpeg("original"), jpeg("the edit"));
    let m1 = h.master("M1", "IMG_0001.JPG", &original).await;
    let original_checksum = checksum_of(&m1);
    h.push(m1);
    let edited = edited_asset(&h, "A1", "M1", &edit).await;
    let edit_checksum = edited["fields"]["resJPEGFullRes"]["value"]["fileChecksum"]
        .as_str()
        .unwrap()
        .to_owned();
    h.push(edited);

    // The previous engine's layout: the edit at the plain name.
    let photos = h.photos();
    std::fs::create_dir_all(photos.join("2023")).unwrap();
    std::fs::write(photos.join("2023/IMG_0001.JPG"), &edit).unwrap();
    std::fs::write(photos.join("2023/IMG_0001_original.JPG"), &original).unwrap();
    let owner = account("upgrade@example.invalid");
    let legacy = h.root.path().join("account.db");
    legacy_state(
        &legacy,
        &owner,
        &[(
            "M1",
            photos.join("2023/IMG_0001_original.JPG"),
            original.len() as u64,
            sha256_hex(&original),
            original_checksum,
        )],
    )
    .await;
    let conn = rusqlite::Connection::open(&legacy).unwrap();
    conn.execute(
        "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes,
             media_type, status, local_path, last_seen_at, local_checksum)
         VALUES ('PrimarySync', 'M1', 'adjusted', ?1, 'IMG_0001.JPG', 0, ?2, 'photo', 'downloaded', ?3, 0, ?4)",
        rusqlite::params![
            edit_checksum,
            edit.len() as i64,
            photos.join("2023/IMG_0001.JPG").to_string_lossy(),
            sha256_hex(&edit)
        ],
    )
    .unwrap();
    drop(conn);
    let target = State::path_for(&legacy);
    upgrade::prepare(&legacy, &target, &owner, &photos)
        .await
        .unwrap()
        .unwrap();

    let state = State::open(&target).await.unwrap();
    let run = async || {
        run_cycle(
            &state,
            &h.libraries(),
            &h.config(),
            &reqwest::Client::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
    };
    let report = run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 0);
    assert_eq!(report.completeness.done, 2);
    assert_eq!(
        sorted_files(&photos),
        ["2023/IMG_0001.JPG", "2023/IMG_0001_original.JPG"]
    );

    // A revert after the upgrade puts the original at the plain name.
    h.push(asset("A1", "M1"));
    let report = run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 1);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), original);
    assert_eq!(h.file("2023/IMG_0001_original.JPG").unwrap(), original);
    let quiet = run().await;
    assert_eq!(quiet.zones[0].drain.downloaded, 0);
    assert_eq!(sorted_files(&photos).len(), 2);
}

#[tokio::test]
async fn only_bytes_kei_downloaded_count_as_original_after_an_upgrade() {
    let h = Harness::new().await;
    let photos = h.photos();
    std::fs::create_dir_all(photos.join("2023")).unwrap();
    let mut legacy_rows = Vec::new();
    for (master, name) in [
        ("M1", "ONE"),
        ("M2", "TWO"),
        ("M3", "THREE"),
        ("M4", "FOUR"),
    ] {
        let bytes = jpeg(name);
        let record = h.master(master, &format!("{name}.JPG"), &bytes).await;
        let checksum = checksum_of(&record);
        h.push(record);
        h.push(asset(&format!("A-{master}"), master));
        std::fs::write(photos.join(format!("2023/{name}.JPG")), &bytes).unwrap();
        if master != "M4" {
            legacy_rows.push((
                master,
                photos.join(format!("2023/{name}.JPG")),
                bytes.len() as u64,
                sha256_hex(&bytes),
                checksum,
            ));
        }
    }
    let owner = account("upgrade@example.invalid");
    let legacy = h.root.path().join("account.db");
    legacy_state(&legacy, &owner, &legacy_rows).await;
    // M1: the previous state proved it downloaded these bytes. M3: what it
    // downloaded differs from what is on disk now. M2 has no proof at all.
    let conn = rusqlite::Connection::open(&legacy).unwrap();
    for (master, name, source) in [
        ("M1", "ONE", sha256_hex(&jpeg("ONE"))),
        (
            "M3",
            "THREE",
            sha256_hex(b"downloaded before a metadata write"),
        ),
    ] {
        conn.execute(
            "INSERT INTO asset_metadata_paths (library, id, version_size, local_path,
                 provider_checksum, local_checksum, source_checksum)
             SELECT library, id, version_size, local_path, checksum, local_checksum, ?2
             FROM assets WHERE id = ?1",
            rusqlite::params![master, source],
        )
        .unwrap();
        let _ = name;
    }
    drop(conn);
    let target = State::path_for(&legacy);
    upgrade::prepare(&legacy, &target, &owner, &photos)
        .await
        .unwrap()
        .unwrap();
    let state = State::open(&target).await.unwrap();
    let report = run_cycle(
        &state,
        &h.libraries(),
        &h.config(),
        &reqwest::Client::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 0);
    assert_eq!(report.zones[0].drain.adopted, 1, "M4 had no record");
    drop(state);

    let db = rusqlite::Connection::open(&target).unwrap();
    let source = |path: &str| -> Option<String> {
        db.query_row(
            "SELECT source_sha FROM files WHERE path = ?1",
            [path],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(source("2023/ONE.JPG"), Some(sha256_hex(&jpeg("ONE"))));
    assert_eq!(source("2023/TWO.JPG"), None);
    assert_eq!(source("2023/THREE.JPG"), None);
    assert_eq!(
        source("2023/FOUR.JPG"),
        None,
        "an adopted file was not downloaded by kei"
    );
}

/// Zone, asset, master, rendition, path, size, SHA-256 and iCloud checksum.
type SavedCopy = (String, String, String, String, String, i64, String, String);

/// Record every copy in `state` the way v0.30.0-beta.3 did: `assets` and
/// `asset_metadata_paths` keyed by the asset record name, and a verified
/// `provider_active_destinations` row naming both the asset and its master,
/// with the path stored as JSON.
fn record_as_beta3(state: &Path, legacy: &Path, directory: &Path) -> usize {
    let new = rusqlite::Connection::open(state).unwrap();
    let mut stmt = new
        .prepare(
            "SELECT f.zone, f.asset, a.master, f.rendition, f.path, f.size, f.sha256, f.checksum
             FROM files f JOIN assets a ON a.zone = f.zone AND a.name = f.asset
             WHERE f.state = 'done'",
        )
        .unwrap();
    let copies: Vec<SavedCopy> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let old = rusqlite::Connection::open(legacy).unwrap();
    // The generation and decision rows a destination references hold
    // nothing the upgrade reads.
    old.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    for (zone, asset, master, rendition, path, size, sha256, checksum) in &copies {
        let absolute = directory.join(path).to_string_lossy().into_owned();
        old.execute(
            "INSERT OR IGNORE INTO assets (library, id, version_size, checksum, filename, created_at,
                 size_bytes, media_type, status, local_path, last_seen_at, local_checksum,
                 download_checksum)
             VALUES (?1, ?2, ?3, ?4, 'x', 0, ?5, 'photo', 'downloaded', ?6, 0, ?7, ?7)",
            rusqlite::params![zone, asset, rendition, checksum, size, absolute, sha256],
        )
        .unwrap();
        old.execute(
            "INSERT INTO asset_metadata_paths (library, id, version_size, local_path,
                 provider_checksum, local_checksum, download_checksum, source_checksum)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6)",
            rusqlite::params![zone, asset, rendition, absolute, checksum, sha256],
        )
        .unwrap();
        old.execute(
            "INSERT INTO provider_active_destinations (generation, pass_key, child, library,
                 asset_id, master, version_size, path, compat_path, checksum, size_bytes,
                 created_at, metadata_hash, admitted, verified_media, verified_metadata,
                 local_checksum, source_checksum, current_publication)
             VALUES ('g1', 'p1', ?2, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 'h', 1, 1, 1, ?9, ?9, 1)",
            rusqlite::params![
                zone,
                asset,
                master,
                rendition,
                json!({"encoding": "utf8", "path": absolute}).to_string(),
                absolute,
                checksum,
                size,
                sha256
            ],
        )
        .unwrap();
    }
    copies.len()
}

#[tokio::test]
async fn an_upgrade_from_the_released_beta_takes_over_every_copy_it_recorded() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    live_photo_named(&h, "M3", "A3", "IMG_0003.JPG").await;
    let m4 = h.master("M4", "IMG_0004.JPG", &jpeg("four")).await;
    h.push(m4);
    h.push(edited_asset(&h, "A4", "M4", &jpeg("four edited")).await);
    let first = h.run().await;
    assert!(first.complete(), "{first:?}");
    let photos = h.photos();
    let before = snapshot(&photos);
    assert_eq!(before.len(), 6, "{:?}", before.keys());

    // Replace the new state with what the released beta recorded for the
    // same files.
    let target = h.root.path().join("state.sync.db");
    let legacy = h.root.path().join("state.db");
    let owner = account("upgrade@example.invalid");
    crate::test_helpers::beta3_state_db(&legacy, &owner);
    let recorded = record_as_beta3(&target, &legacy, &photos);
    assert_eq!(recorded, 6);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", target.display()));
    }

    let import = upgrade::prepare(&legacy, &target, &owner, &photos)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(import.recorded, 3 * recorded as u64);
    assert_eq!(import.imported, recorded as u64);
    assert_eq!(import.duplicates, 2 * recorded as u64);
    assert_eq!(
        (import.unreadable, import.ambiguous, import.changed_on_disk),
        (0, 0, 0)
    );

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.zones[0].drain.downloaded, 0);
    assert_eq!(report.zones[0].drain.adopted, 0);
    assert_eq!(report.completeness.done, recorded as u64);
    assert_eq!(snapshot(&photos), before, "no file was added or changed");
    let state = h.state().await;
    assert!(state.list_imported().await.unwrap().is_empty());
    assert!(upgrade::finish(&state).await.unwrap().is_some());
    assert!(!legacy.exists());
    drop(state);
    let db = rusqlite::Connection::open(&target).unwrap();
    // Each table proved the bytes kei downloaded.
    let unproven: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM files WHERE source_sha IS NULL OR source_sha != sha256",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unproven, 0);
}

#[test]
fn a_destination_path_decodes_from_the_released_beta_encoding() {
    use super::upgrade::destination_path;
    assert_eq!(
        destination_path(r#"{"encoding":"utf8","path":"/photos/a \"b\".jpg"}"#).as_deref(),
        Some("/photos/a \"b\".jpg")
    );
    assert_eq!(
        destination_path(r#"{"encoding":"unix_bytes","path":[47,97]}"#).as_deref(),
        Some("/a")
    );
    assert_eq!(
        destination_path(r#"{"encoding":"unix_bytes","path":[255]}"#),
        None
    );
    assert_eq!(
        destination_path(r#"{"encoding":"windows_units","path":[67,58]}"#).as_deref(),
        Some("C:")
    );
    assert_eq!(
        destination_path("/photos/plain.jpg").as_deref(),
        Some("/photos/plain.jpg")
    );
    assert_eq!(destination_path("{not json"), None);
}

#[tokio::test]
async fn an_upgrade_refuses_another_accounts_state() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("account.db");
    legacy_state(&legacy, &account("a@example.invalid"), &[]).await;
    let before = std::fs::read(&legacy).unwrap();
    let target = State::path_for(&legacy);
    let error = upgrade::prepare(&legacy, &target, &account("b@example.invalid"), root.path())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("account ownership does not match"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(&legacy).unwrap(), before);
    assert!(!target.exists());
}

#[tokio::test]
async fn an_upgrade_reads_the_released_v0240_schema_after_ownership_is_bound() {
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    let legacy = root.path().join("account.db");
    let conn = rusqlite::Connection::open(&legacy).unwrap();
    conn.execute_batch(include_str!("../../tests/data/released-v0240-schema.sql"))
        .unwrap();
    // `kei migrate-state` binds this header to a released, unowned file.
    let owner = account("released@example.invalid");
    conn.execute_batch(
        "CREATE TABLE account_owner (singleton INTEGER PRIMARY KEY CHECK(singleton=1),
             version INTEGER NOT NULL, account_key TEXT NOT NULL, provider_key TEXT NOT NULL);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO account_owner VALUES (1, 1, ?1, ?2)",
        rusqlite::params![owner.key(), owner.provider_key().unwrap()],
    )
    .unwrap();
    for (id, name, bytes) in [
        ("same", "original.jpg", b"original".as_slice()),
        ("other", "other.jpg", b"other bytes"),
    ] {
        let path = media.join(name);
        std::fs::write(&path, bytes).unwrap();
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes,
                 media_type, status, local_path, last_seen_at, local_checksum)
             VALUES ('PrimarySync', ?1, 'original', 'AQID', ?2, 0, ?3, 'photo', 'downloaded', ?4, 0, ?5)",
            rusqlite::params![id, name, bytes.len() as i64, path.to_string_lossy(), sha256_hex(bytes)],
        )
        .unwrap();
    }
    drop(conn);

    let target = State::path_for(&legacy);
    let import = upgrade::prepare(&legacy, &target, &owner, &media)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (import.recorded, import.imported, import.changed_on_disk),
        (2, 2, 0)
    );
    let state = State::open(&target).await.unwrap();
    let copies = state.list_imported().await.unwrap();
    assert_eq!(
        copies
            .iter()
            .map(|copy| copy.path.as_str())
            .collect::<Vec<_>>(),
        ["original.jpg", "other.jpg"]
    );
    assert_eq!(
        copies[0].sha256.as_deref(),
        Some(sha256_hex(b"original").as_str())
    );

    // Another download directory leaves the recorded copies outside it.
    let elsewhere = root.path().join("new-destination");
    let other_target = root.path().join("other.sync.db");
    let import = upgrade::prepare(&legacy, &other_target, &owner, &elsewhere)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((import.imported, import.outside), (0, 2));
}

#[tokio::test]
async fn confirmed_damage_is_replaced_by_the_next_sync_only_while_unchanged() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;
    let path = h.photos().join("2023/IMG_0001.JPG");
    std::fs::write(&path, b"\xFF\xD8\xFF\xE0damaged").unwrap();
    let state = h.state().await;
    let row = state.file_at("2023/IMG_0001.JPG").await.unwrap().unwrap();
    // What `reconcile --confirm-damage` records: the exact damaged bytes.
    state
        .owe_again(row.id, Some(&sha256_hex(b"\xFF\xD8\xFF\xE0damaged")))
        .await
        .unwrap();
    drop(state);

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("one"));

    // A file edited again after confirmation is kept.
    let state = h.state().await;
    let row = state.file_at("2023/IMG_0001.JPG").await.unwrap().unwrap();
    state
        .owe_again(row.id, Some(&sha256_hex(b"confirmed")))
        .await
        .unwrap();
    drop(state);
    let report = h.run().await;
    assert_eq!(report.completeness.modified, 1);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("one"));
}

/// Libraries written by icloudpd are adopted in place by `import-existing`.
/// Paths, sizes and names come from icloudpd's own test fixtures; nothing is
/// downloaded, so a divergence shows up as an owed copy.
mod icloudpd_layout {
    use super::*;
    use crate::download::paths;
    use chrono::TimeZone;

    /// Capture time that renders as `folder` (`Y/M/D`) in local time; 18:00
    /// keeps daylight-saving changes from crossing midnight.
    fn local_day_ms(folder: &str) -> f64 {
        let parts: Vec<u32> = folder.split('/').map(|s| s.parse().unwrap()).collect();
        chrono::Local
            .with_ymd_and_hms(parts[0] as i32, parts[1], parts[2], 18, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis() as f64
    }

    fn item_type(filename: &str) -> &'static str {
        let lower = filename.to_lowercase();
        if lower.ends_with(".heic") {
            "public.heic"
        } else if lower.ends_with(".mov") {
            "com.apple.quicktime-movie"
        } else if lower.ends_with(".mp4") {
            "public.mpeg-4"
        } else if lower.ends_with(".dng") {
            "com.adobe.raw-image"
        } else {
            "public.jpeg"
        }
    }

    fn photo(h: &Harness, name: &str, filename: &str, size: u64, folder: &str, mov: Option<u64>) {
        let kind = item_type(filename);
        let mut fields = json!({
            "filenameEnc": {"value": filename, "type": "STRING"},
            "itemType": {"value": kind},
            "resOriginalFileType": {"value": kind},
            "resOriginalRes": {"value": {
                "size": size, "fileChecksum": format!("ck-{name}"),
                "downloadURL": "https://p01.icloud-content.com/never-fetched"
            }}
        });
        if let Some(mov) = mov {
            fields["resOriginalVidComplRes"] = json!({"value": {
                "size": mov, "fileChecksum": format!("ck-{name}-mov"),
                "downloadURL": "https://p01.icloud-content.com/never-fetched-mov"
            }});
            fields["resOriginalVidComplFileType"] = json!({"value": "com.apple.quicktime-movie"});
        }
        h.push(json!({"recordName": name, "recordType": "CPLMaster", "fields": fields}));
        let mut child = asset(&format!("{name}-asset"), name);
        child["fields"]["assetDate"] = json!({"value": local_day_ms(folder)});
        h.push(child);
    }

    fn stage(h: &Harness, relative: &str, size: u64) {
        let path = h.photos().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![0_u8; usize::try_from(size).unwrap()]).unwrap();
    }

    fn dated(config: &mut CoreConfig) {
        config.transfer = Transfer::AdoptOnly;
        config.paths.folder_structure = "%Y/%m/%d".into();
    }

    fn dated_id7(config: &mut CoreConfig) {
        dated(config);
        config.paths.file_match_policy = crate::types::FileMatchPolicy::NameId7;
    }

    fn flat(config: &mut CoreConfig) {
        config.transfer = Transfer::AdoptOnly;
        config.paths.folder_structure = String::new();
    }

    fn yearly(config: &mut CoreConfig) {
        config.transfer = Transfer::AdoptOnly;
        config.paths.folder_structure = "%Y".into();
    }

    /// Adopt every staged file and download nothing.
    async fn adopts_all(h: &Harness, tweak: fn(&mut CoreConfig), copies: u64) {
        *h.tweak.lock().unwrap() = Some(tweak);
        let before = sorted_files(&h.photos());
        let report = h.run().await;
        assert_eq!(report.zones[0].drain.downloaded, 0);
        assert_eq!(
            (report.completeness.done, report.completeness.pending),
            (copies, 0),
            "{report:?}"
        );
        assert_eq!(sorted_files(&h.photos()), before);
    }

    #[tokio::test]
    async fn default_layout_and_size_suffixed_collisions() {
        let h = Harness::new().await;
        for (name, folder, filename, size) in [
            ("DEF0", "2018/07/30", "IMG_7408.JPG", 1_151_066),
            ("DEF1", "2018/07/30", "IMG_7407.JPG", 656_257),
            ("DEF2", "2018/07/31", "IMG_7409.JPG", 100_000),
            ("DUP1", "2018/07/30", "IMG_DUP.JPG", 100_000),
            ("DUP2", "2018/07/30", "IMG_DUP.JPG", 200_000),
        ] {
            photo(&h, name, filename, size, folder, None);
        }
        for (relative, size) in [
            ("2018/07/30/IMG_7408.JPG", 1_151_066),
            ("2018/07/30/IMG_7407.JPG", 656_257),
            ("2018/07/31/IMG_7409.JPG", 100_000),
            ("2018/07/30/IMG_DUP.JPG", 100_000),
            ("2018/07/30/IMG_DUP-200000.JPG", 200_000),
        ] {
            stage(&h, relative, size);
        }
        adopts_all(&h, dated, 5).await;
    }

    #[tokio::test]
    async fn name_id7_layout() {
        let h = Harness::new().await;
        for (name, folder, filename, size) in [
            ("REC_FOR_7408", "2018/07/30", "IMG_7408.JPG", 1_151_066),
            ("REC_FOR_7409", "2018/07/31", "IMG_7409.JPG", 100_000),
        ] {
            photo(&h, name, filename, size, folder, None);
            stage(
                &h,
                &format!("{folder}/{}", paths::apply_name_id7(filename, name)),
                size,
            );
        }
        adopts_all(&h, dated_id7, 2).await;
    }

    #[tokio::test]
    async fn live_photo_layouts() {
        let h = Harness::new().await;
        photo(
            &h,
            "LIVEJ",
            "IMG_7408.JPG",
            1_151_066,
            "2018/07/30",
            Some(2_000_000),
        );
        photo(
            &h,
            "LIVEH",
            "IMG_5000.HEIC",
            800_000,
            "2018/07/30",
            Some(3_000_000),
        );
        for (relative, size) in [
            ("2018/07/30/IMG_7408.JPG", 1_151_066),
            ("2018/07/30/IMG_7408.MOV", 2_000_000),
            ("2018/07/30/IMG_5000.HEIC", 800_000),
            ("2018/07/30/IMG_5000_HEVC.MOV", 3_000_000),
        ] {
            stage(&h, relative, size);
        }
        adopts_all(&h, dated, 4).await;

        let h = Harness::new().await;
        photo(
            &h,
            "REC_LIVE_HEIC",
            "IMG_5000.HEIC",
            800_000,
            "2018/07/30",
            Some(3_000_000),
        );
        let image = paths::apply_name_id7("IMG_5000.HEIC", "REC_LIVE_HEIC");
        let movie = paths::live_photo_mov_path_suffix(&image);
        stage(&h, &format!("2018/07/30/{image}"), 800_000);
        stage(&h, &format!("2018/07/30/{movie}"), 3_000_000);
        adopts_all(&h, dated_id7, 2).await;
    }

    #[tokio::test]
    async fn video_raw_and_folder_structure_layouts() {
        let h = Harness::new().await;
        photo(&h, "VID0", "IMG_VID1.MOV", 5_000_000, "2018/07/30", None);
        photo(&h, "VID1", "IMG_VID2.MP4", 10_000_000, "2018/07/31", None);
        photo(&h, "RAW0", "IMG_RAW.DNG", 25_000_000, "2018/07/31", None);
        stage(&h, "2018/07/30/IMG_VID1.MOV", 5_000_000);
        stage(&h, "2018/07/31/IMG_VID2.MP4", 10_000_000);
        stage(&h, "2018/07/31/IMG_RAW.DNG", 25_000_000);
        adopts_all(&h, dated, 3).await;

        let h = Harness::new().await;
        photo(&h, "FLAT1", "IMG_FLAT1.JPG", 100_000, "2018/07/30", None);
        photo(&h, "FLAT2", "IMG_FLAT2.JPG", 200_000, "2018/07/30", None);
        stage(&h, "IMG_FLAT1.JPG", 100_000);
        stage(&h, "IMG_FLAT2.JPG", 200_000);
        adopts_all(&h, flat, 2).await;

        let h = Harness::new().await;
        photo(&h, "Y1", "IMG_Y1.JPG", 100_000, "2018/07/30", None);
        photo(&h, "Y2", "IMG_Y2.JPG", 200_000, "2018/12/31", None);
        stage(&h, "2018/IMG_Y1.JPG", 100_000);
        stage(&h, "2018/IMG_Y2.JPG", 200_000);
        adopts_all(&h, yearly, 2).await;
    }
}

/// The opt-in metadata pass runs after downloads and only on copies that
/// still hold the bytes kei recorded.
#[cfg(feature = "xmp")]
mod metadata_pass {
    use super::*;
    use crate::download::metadata_pass::{CaptureTimestampRepair, MetadataFlags, Settings};
    use crate::sync_core::state::FileState;

    /// SOI + APP0 JFIF + EOI: a JPEG the XMP toolkit can write into.
    fn small_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    fn rating(config: &mut CoreConfig) {
        config.metadata = Some(Settings {
            flags: MetadataFlags::RATING | MetadataFlags::DATETIME,
            repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".kei-part".into(),
        });
    }

    fn favorite(name: &str, master: &str, on: bool) -> Value {
        let mut record = asset(name, master);
        record["fields"]["isFavorite"] = json!({"value": i64::from(on)});
        record
    }

    async fn recorded(h: &Harness, path: &str) -> (Option<String>, FileState) {
        let row = h.state().await.file_at(path).await.unwrap().unwrap();
        (row.sha256, row.state)
    }

    #[tokio::test]
    async fn metadata_is_written_after_download_then_stays_quiet_until_icloud_changes() {
        let h = Harness::new().await;
        *h.tweak.lock().unwrap() = Some(rating);
        let m1 = h.master("M1", "IMG_0001.JPG", &small_jpeg()).await;
        h.push(m1);
        h.push(favorite("A1", "M1", true));

        let first = h.run().await;
        assert!(first.complete(), "{first:?}");
        assert_eq!(first.zones[0].drain.downloaded, 1);
        assert_eq!(first.zones[0].metadata.written, 1, "{first:?}");
        let bytes = h.file("2023/IMG_0001.JPG").unwrap();
        assert_ne!(bytes, small_jpeg(), "the rating was embedded");
        // kei's own rewrite is the recorded copy, not a local edit.
        assert_eq!(
            recorded(&h, "2023/IMG_0001.JPG").await,
            (Some(sha256_hex(&bytes)), FileState::Done)
        );

        let quiet = h.run().await;
        assert_eq!(quiet.zones[0].metadata, Default::default());
        assert_eq!(quiet.zones[0].drain.downloaded, 0);
        assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), bytes);

        // A change in iCloud reaches the file without a download.
        h.push(favorite("A1", "M1", false));
        let changed = h.run().await;
        assert!(changed.complete(), "{changed:?}");
        assert_eq!(changed.zones[0].drain.downloaded, 0);
        assert_eq!(changed.zones[0].metadata.written, 1, "{changed:?}");
        let rewritten = h.file("2023/IMG_0001.JPG").unwrap();
        assert_ne!(rewritten, bytes);
        assert_eq!(
            recorded(&h, "2023/IMG_0001.JPG").await,
            (Some(sha256_hex(&rewritten)), FileState::Done)
        );
    }

    #[tokio::test]
    async fn primary_naming_hands_over_a_copy_kei_wrote_metadata_into() {
        let h = Harness::new().await;
        *h.tweak.lock().unwrap() = Some(|config| {
            rating(config);
            config.paths.edited_naming = crate::types::EditedNaming::Primary;
        });
        let m1 = h.master("M1", "IMG_0001.JPG", &small_jpeg()).await;
        h.push(m1);
        h.push(favorite("A1", "M1", true));
        let first = h.run().await;
        assert_eq!(first.zones[0].metadata.written, 1, "{first:?}");

        // The edit replaces the plain name even though kei rewrote it for
        // metadata: those bytes are kei's own.
        let mut edit = small_jpeg();
        edit[15] = 0x02; // another valid JPEG: a different pixel density
        let mut edited = edited_asset(&h, "A1", "M1", &edit).await;
        edited["fields"]["isFavorite"] = json!({"value": 1});
        h.push(edited);
        let report = h.run().await;
        assert!(report.complete(), "{report:?}");
        assert_eq!(report.completeness.modified, 0);
        assert_eq!(report.zones[0].drain.downloaded, 2);
        assert_eq!(report.zones[0].metadata.written, 2, "{report:?}");
        for path in ["2023/IMG_0001.JPG", "2023/IMG_0001_original.JPG"] {
            let bytes = h.file(path).unwrap();
            assert_eq!(
                recorded(&h, path).await,
                (Some(sha256_hex(&bytes)), FileState::Done),
                "{path}"
            );
        }
        let plain = h.file("2023/IMG_0001.JPG").unwrap();
        assert_ne!(plain, edit, "the edit got its rating too");
        assert_eq!(plain[15], 0x02, "the plain name holds the edit");

        let quiet = h.run().await;
        assert_eq!(quiet.zones[0].metadata, Default::default());
        assert_eq!(quiet.zones[0].drain, drain::DrainStats::default());
    }

    fn sidecar(config: &mut CoreConfig) {
        config.metadata = Some(Settings {
            flags: MetadataFlags::XMP_SIDECAR | MetadataFlags::INCLUDE_ALBUMS,
            repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".kei-part".into(),
        });
    }

    #[tokio::test]
    async fn a_sidecar_carries_album_names_and_the_media_is_untouched() {
        let h = Harness::new().await;
        *h.tweak.lock().unwrap() = Some(sidecar);
        h.push(album("AL", "Trip"));
        let m1 = h.master("M1", "IMG_0001.JPG", &small_jpeg()).await;
        h.push(m1);
        h.push(favorite("A1", "M1", true));
        h.push(member("A1", "AL"));

        let report = h.run().await;
        assert!(report.complete(), "{report:?}");
        assert_eq!(report.zones[0].metadata.written, 1, "{report:?}");
        assert_eq!(h.file("Trip/IMG_0001.JPG").unwrap(), small_jpeg());
        let xmp = String::from_utf8(h.file("Trip/IMG_0001.JPG.xmp").unwrap()).unwrap();
        assert!(xmp.contains("Trip"), "{xmp}");
        let quiet = h.run().await;
        assert_eq!(quiet.zones[0].metadata, Default::default());
    }

    #[tokio::test]
    async fn a_locally_edited_copy_gets_no_metadata() {
        let h = Harness::new().await;
        let m1 = h.master("M1", "IMG_0001.JPG", &small_jpeg()).await;
        h.push(m1);
        h.push(favorite("A1", "M1", true));
        h.run().await;
        let edited = [small_jpeg(), b"edited".to_vec()].concat();
        std::fs::write(h.photos().join("2023/IMG_0001.JPG"), &edited).unwrap();

        *h.tweak.lock().unwrap() = Some(rating);
        let report = h.run().await;
        assert_eq!(report.zones[0].metadata.modified, 1, "{report:?}");
        assert_eq!(report.completeness.modified, 1);
        assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), edited);
    }

    #[tokio::test]
    async fn kei_bytes_published_before_a_crash_are_not_taken_for_an_edit() {
        let h = Harness::new().await;
        let m1 = h.master("M1", "IMG_0001.JPG", &small_jpeg()).await;
        h.push(m1);
        h.push(favorite("A1", "M1", true));
        h.run().await;
        // A metadata rewrite was published, then kei stopped before recording it.
        let published = [small_jpeg(), b"kei-written".to_vec()].concat();
        let state = h.state().await;
        let row = state.file_at("2023/IMG_0001.JPG").await.unwrap().unwrap();
        state
            .set_pending(row.id, &sha256_hex(&published), Some("{}".into()))
            .await
            .unwrap();
        drop(state);
        std::fs::write(h.photos().join("2023/IMG_0001.JPG"), &published).unwrap();

        *h.tweak.lock().unwrap() = Some(rating);
        let report = h.run().await;
        assert_eq!(report.zones[0].metadata.modified, 0, "{report:?}");
        assert_eq!(report.completeness.modified, 0);
        let (sha, state) = recorded(&h, "2023/IMG_0001.JPG").await;
        assert_eq!(state, FileState::Done);
        assert_eq!(sha, Some(sha256_hex(&h.file("2023/IMG_0001.JPG").unwrap())));
    }
}

#[tokio::test]
async fn a_replacement_published_before_a_crash_is_not_downloaded_again() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    h.run().await;
    let m1 = h.master("M1", "IMG_0001.JPG", &jpeg("one v2")).await;
    h.push(m1);
    // The new version landed, then kei stopped before recording it.
    let state = h.state().await;
    let row = state.file_at("2023/IMG_0001.JPG").await.unwrap().unwrap();
    state
        .set_pending(row.id, &sha256_hex(&jpeg("one v2")), None)
        .await
        .unwrap();
    drop(state);
    std::fs::write(h.photos().join("2023/IMG_0001.JPG"), jpeg("one v2")).unwrap();

    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    assert_eq!(report.completeness.modified, 0);
    assert_eq!(h.file("2023/IMG_0001.JPG").unwrap(), jpeg("one v2"));
    let row = h
        .state()
        .await
        .file_at("2023/IMG_0001.JPG")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.sha256, Some(sha256_hex(&jpeg("one v2"))));
}

#[tokio::test]
#[tracing_test::traced_test]
async fn long_steps_report_progress_with_what_is_saved_and_what_remains() {
    let h = Harness::new().await;
    library_with_album(&h).await;
    let report = h.run().await;
    assert!(report.complete(), "{report:?}");
    // A quiet second cycle lists one page and owes nothing, so it adds none
    // of these lines.
    h.run().await;
    logs_assert(|lines: &[&str]| {
        let once = |needle: &str, fields: &[&str]| {
            let found: Vec<&&str> = lines.iter().filter(|line| line.contains(needle)).collect();
            match found.as_slice() {
                [line] if fields.iter().all(|field| line.contains(field)) => Ok(()),
                _ => Err(format!(
                    "expected one {needle:?} line with {fields:?}: {found:?}"
                )),
            }
        };
        once("Listing the whole library", &[])?;
        once("Listing library: progress saved", &["pages=1"])?;
        once("Saving owed copies", &["owed=2"])?;
        once("Saved copies", &["downloaded=2", "remaining=0"])
    });
}
