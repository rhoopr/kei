//! Two fixed six-cycle histories, observed through equivalent provider encodings.
//! Replays retain the prerequisite/recovery/UNRESOLVED_RETRY tail; no unsafe reducer.
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoLibrary, PhotosSession};
use crate::sync_cycle::sync_token_key;
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};

const ZONES: [&str; 2] = ["PrimarySync", "SharedSync-MIXED"];
const CYCLES: usize = 6;

// The two UNRESOLVED_RETRY tails replay the selected unknown asset. The bound
// is structural, not a watermark: per zone the selection ledger reads at most
// one retained root (`recover_retained`, `retained_selection_root` LIMIT 1) and
// at most one current root (`replay` pass loop, `current_download_generation`
// LIMIT 1). The two selected album passes (B and C) each carry the unknown
// child; the Unfiled pass excludes it. Each visit is confirmed twice at source
// level: `prepare_replay` calls `confirm_layout_asset`, then the download
// producer calls `SelectionRun::confirm` again. `confirm_catalog_asset` always
// issues the child lookup followed by the child+master pair lookup.
const SELECTED_ALBUM_PASSES_PER_ZONE: usize = 2;
const REPLAY_ROOTS_PER_ZONE: usize = 2;
const SOURCE_CONFIRMATIONS_PER_VISIT: usize = 2;
const LOOKUPS_PER_SOURCE_CONFIRMATION: usize = 2;
const VISITS_PER_ZONE: usize = SELECTED_ALBUM_PASSES_PER_ZONE * REPLAY_ROOTS_PER_ZONE;
const LOOKUPS_PER_VISIT: usize = SOURCE_CONFIRMATIONS_PER_VISIT * LOOKUPS_PER_SOURCE_CONFIRMATION;
const UNRESOLVED_RETRY_LOOKUP_CAP_PER_ZONE: usize = VISITS_PER_ZONE * LOOKUPS_PER_VISIT;
const UNRESOLVED_RETRY_LOOKUP_CAP: usize = ZONES.len() * UNRESOLVED_RETRY_LOOKUP_CAP_PER_ZONE;
// One empty changes delta plus three CheckIndexingState readiness queries per
// zone are protocol traffic, not DATA record pages.
const UNRESOLVED_RETRY_EMPTY_DELTAS_PER_ZONE: usize = 1;
const UNRESOLVED_RETRY_READINESS_QUERIES_PER_ZONE: usize = 3;
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

const OWNER_RECORD_NAME: &str = "_defaultOwner";
const FAULT_REASON: &str = "icloud_changes_successor_missing";
const MALFORMED_ASSET_REASON: &str = "malformed_required_asset_fields";

fn mixed_owner() -> crate::state::db::account::AccountOwner {
    crate::state::db::account::AccountOwner::authenticated(
        "mixed@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-mixed"}}))
            .expect("synthetic mixed account login response"),
    )
    .expect("synthetic mixed account owner")
}

fn zone_scope(zone: &str) -> Value {
    json!({"zoneName":zone,"ownerRecordName":OWNER_RECORD_NAME})
}

fn zone_boundary(zone: &str) -> String {
    format!("mixed-{zone}-boundary")
}

fn zone_token(zone: &str, cycle: usize) -> String {
    format!("mixed-{zone}-c{cycle}")
}

fn record_page_cursor(zone: &str, cycle: usize, page: usize) -> String {
    format!("mixed-{zone}-c{cycle}-p{page}")
}

fn terminal_cursor(zone: &str, cycle: usize) -> String {
    format!("mixed-{zone}-c{cycle}-t")
}

fn successor_cursor(zone: &str, cycle: usize) -> String {
    zone_token(zone, (cycle + 1).min(CYCLES - 2))
}

// Durable start token for the cycle. IncompleteInventory keeps the same
// config, so only its current-delta fault holds PrimarySync. SelectionChange
// changes rendition mode, so the real full reconciliation stages the healthy
// peer token and promotes both zones only after the second complete pass.
fn token_tables(history: History) -> ([[usize; 6]; 2], [[usize; 6]; 2]) {
    match history {
        History::IncompleteInventory => (
            [[0, 0, 0, 3, 4, 4], [0, 1, 2, 3, 4, 4]],
            [[0, 0, 3, 4, 4, 4], [1, 2, 3, 4, 4, 4]],
        ),
        History::SelectionChange => (
            [[0, 0, 0, 3, 4, 4], [0, 1, 1, 3, 4, 4]],
            [[0, 0, 3, 4, 4, 4], [1, 1, 3, 4, 4, 4]],
        ),
    }
}

fn start_token(history: History, zone: usize, cycle: usize) -> String {
    let (start, _) = token_tables(history);
    zone_token(ZONES[zone], start[zone][cycle])
}

fn durable_token_after(history: History, zone: usize, cycle: usize) -> String {
    let (_, after) = token_tables(history);
    zone_token(ZONES[zone], after[zone][cycle])
}

fn expected_downloads(history: History, cycle: usize) -> usize {
    match (history, cycle) {
        (History::IncompleteInventory, 0) => 2,
        (History::SelectionChange, 0) => 1,
        (History::SelectionChange, 2) => 1,
        _ => 0,
    }
}

fn expected_block_reason(history: History, cycle: usize) -> Option<&'static str> {
    match (history, cycle) {
        (History::IncompleteInventory, 0 | 1) => Some(FAULT_REASON),
        (History::SelectionChange, 0) => Some(MALFORMED_ASSET_REASON),
        (History::SelectionChange, 1) => Some("icloud_sync_token_missing"),
        _ => None,
    }
}

fn page_response(zone: &str, records: Vec<Value>, token: &str, more_coming: bool) -> Value {
    json!({"zones":[{"zoneID":zone_scope(zone),"records":records,"syncToken":token,"moreComing":more_coming}]})
}

fn album_query_records(records: &[Value]) -> Vec<Value> {
    records.get(3..5).map(<[Value]>::to_vec).unwrap_or_default()
}

fn unfiled_query_records(records: &[Value]) -> Vec<Value> {
    let mut selected = records.get(..3).map(<[Value]>::to_vec).unwrap_or_default();
    if let Some(live) = records.get(5..) {
        selected.extend_from_slice(live);
    }
    selected
}

fn expected_record_page_sizes(encoding: Encoding, record_count: usize) -> Vec<usize> {
    fn chunked(count: usize) -> Vec<usize> {
        let mut pages = Vec::new();
        let mut remaining = count;
        while remaining > 2 {
            pages.push(2);
            remaining -= 2;
        }
        if remaining > 0 {
            pages.push(remaining);
        }
        pages
    }
    match encoding {
        Encoding::Baseline => vec![record_count],
        Encoding::PageCuts => chunked(record_count),
        Encoding::DuplicateEmpty => {
            let mut pages = chunked(record_count * 2);
            pages.insert(1, 0);
            pages
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageRole {
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultKind {
    MissingSuccessor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FaultWitness {
    zone: &'static str,
    cycle: usize,
    incoming_cursor: String,
    page_role: PageRole,
    fault_kind: FaultKind,
}

type PageHit = (&'static str, usize, usize);

struct ProviderInput {
    zone: &'static str,
    records: Vec<Value>,
    query_records: Vec<Value>,
    encoding: Encoding,
    unresolved_retry_tail: bool,
    requests: Arc<AtomicUsize>,
    hydrations: Arc<AtomicUsize>,
    tail_changes: Arc<AtomicUsize>,
    tail_readiness: Arc<AtomicUsize>,
    tail_data_queries: Arc<AtomicUsize>,
    tail_lookup_sequence: Arc<AtomicUsize>,
    priming: bool,
    cycle: usize,
    expected_cursor: Option<String>,
    fault_active: bool,
    witness: Arc<Mutex<Vec<FaultWitness>>>,
    page_hits: Arc<Mutex<Vec<PageHit>>>,
}

#[derive(Clone, Debug)]
struct Provider {
    zone: &'static str,
    records: Vec<Value>,
    query_records: Vec<Value>,
    encoding: Encoding,
    unresolved_retry_tail: bool,
    requests: Arc<AtomicUsize>,
    hydrations: Arc<AtomicUsize>,
    tail_changes: Arc<AtomicUsize>,
    tail_readiness: Arc<AtomicUsize>,
    tail_data_queries: Arc<AtomicUsize>,
    tail_lookup_sequence: Arc<AtomicUsize>,
    priming: bool,
    cycle: usize,
    expected_cursor: Option<String>,
    fault_active: bool,
    next_expected: Arc<Mutex<Option<String>>>,
    next_page: Arc<Mutex<usize>>,
    witness: Arc<Mutex<Vec<FaultWitness>>>,
    page_hits: Arc<Mutex<Vec<PageHit>>>,
}

fn provider(input: ProviderInput) -> Provider {
    Provider {
        zone: input.zone,
        records: input.records,
        query_records: input.query_records,
        encoding: input.encoding,
        unresolved_retry_tail: input.unresolved_retry_tail,
        requests: input.requests,
        hydrations: input.hydrations,
        tail_changes: input.tail_changes,
        tail_readiness: input.tail_readiness,
        tail_data_queries: input.tail_data_queries,
        tail_lookup_sequence: input.tail_lookup_sequence,
        priming: input.priming,
        cycle: input.cycle,
        next_expected: Arc::new(Mutex::new(input.expected_cursor.clone())),
        expected_cursor: input.expected_cursor,
        fault_active: input.fault_active,
        next_page: Arc::new(Mutex::new(0)),
        witness: input.witness,
        page_hits: input.page_hits,
    }
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

    fn history_page(&self, cursor: Option<&str>) -> Value {
        let expected = self
            .next_expected
            .lock()
            .expect("zone-history cursor lock")
            .clone()
            .expect("zone-history cursor is initialized");
        assert_eq!(
            cursor,
            Some(expected.as_str()),
            "changes/zone must continue from the admitted predecessor"
        );
        if self.unresolved_retry_tail {
            // The UNRESOLVED_RETRY tail keeps the same current token: no new
            // checkpoint progress and no new logical DATA delta page.
            return page_response(self.zone, Vec::new(), &expected, false);
        }
        let pages = self.pages();
        let mut next_page = self.next_page.lock().expect("zone-history page lock");
        let index = *next_page;
        if index < pages.len() {
            let token = if index + 1 == pages.len() {
                terminal_cursor(self.zone, self.cycle)
            } else {
                record_page_cursor(self.zone, self.cycle, index)
            };
            *next_page += 1;
            self.page_hits
                .lock()
                .expect("record page witness lock")
                .push((self.zone, self.cycle, pages[index].len()));
            // The token is distinct for every record page, and the final
            // record page hands over to the terminal cursor.
            *self.next_expected.lock().expect("zone-history cursor lock") = Some(token.clone());
            return page_response(self.zone, pages[index].clone(), &token, true);
        }
        *next_page += 1;
        let token = successor_cursor(self.zone, self.cycle);
        if self.fault_active {
            // The only injected fault: every logical record page above was
            // served, then the terminal page omits its successor. The parser
            // classifies this as MissingChangesSuccessor so admitted work stays
            // durable while the prior cursor is held.
            self.witness
                .lock()
                .expect("fault witness lock")
                .push(FaultWitness {
                    zone: self.zone,
                    cycle: self.cycle,
                    incoming_cursor: expected,
                    page_role: PageRole::Terminal,
                    fault_kind: FaultKind::MissingSuccessor,
                });
            *self.next_expected.lock().expect("zone-history cursor lock") = None;
            return json!({"zones":[{"zoneID":zone_scope(self.zone),"records":[],"moreComing":false}]});
        }
        *self.next_expected.lock().expect("zone-history cursor lock") = Some(token.clone());
        page_response(self.zone, Vec::new(), &token, false)
    }
}

#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let count = self.requests.fetch_add(1, Ordering::SeqCst);
        assert!(count < 300, "bounded provider work per history");
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            assert!(self.priming, "baseline boundary is only used by priming");
            assert_eq!(request, json!({}), "unexpected zone boundary request");
            return Ok(json!({"zones":[{"zoneID":zone_scope(self.zone),
                "syncToken":zone_boundary(self.zone),"deleted":false}]}));
        }
        if url.contains("/changes/zone?") {
            self.tail_changes.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                request["zones"][0]["zoneID"]["zoneName"], self.zone,
                "changes/zone scope"
            );
            assert_eq!(
                request["zones"][0]["zoneID"]["ownerRecordName"], OWNER_RECORD_NAME,
                "changes/zone owner scope"
            );
            let cursor = request["zones"][0]["syncToken"].as_str();
            if self.priming {
                assert_eq!(cursor, Some(zone_boundary(self.zone).as_str()));
                return Ok(page_response(
                    self.zone,
                    Vec::new(),
                    &zone_token(self.zone, 0),
                    false,
                ));
            }
            if self.expected_cursor.is_some() {
                return Ok(self.history_page(cursor));
            }
            // Legacy provider shape retained for the reduced regressions in
            // this file: a stored non-page cursor is a complete empty delta.
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
            return Ok(page_response(self.zone, records, &token, !final_page));
        }
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response((self.query_records.len() / 2) as u64));
        }
        if url.contains("/records/query?") {
            if request["query"]["recordType"].as_str() == Some("CheckIndexingState") {
                self.tail_readiness.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"records":[{"recordName":"indexing",
                    "recordType":"CheckIndexingState",
                    "fields":{"state":{"value":"FINISHED"}}}]}));
            }
            self.tail_data_queries.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let token = if self.priming {
                format!("mixed-{}-query-eof", self.zone)
            } else {
                "after".to_string()
            };
            return Ok(
                json!({"records":if offset == 0 {self.query_records.clone()} else {Vec::new()},"syncToken":token}),
            );
        }
        assert!(
            url.contains("/records/lookup?"),
            "unexpected provider route {url}"
        );
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        let requested_names: Vec<&str> = request["records"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|record| record["recordName"].as_str().unwrap_or(""))
            .collect();
        if self.unresolved_retry_tail {
            // The approved coverage change allows only the exact selected
            // unknown child to hydrate in the tail. This applies to every
            // original history and encoding, not only the diagnostic replay.
            // Any other route, zone, owner, identity, or ordering fails.
            assert!(
                ZONES.contains(&self.zone),
                "UNRESOLVED_RETRY tail zone identity: {}",
                self.zone
            );
            assert_eq!(
                request["zoneID"]["zoneName"].as_str(),
                Some(self.zone),
                "UNRESOLVED_RETRY tail lookup zone"
            );
            assert_eq!(
                request["zoneID"]["ownerRecordName"].as_str(),
                Some(OWNER_RECORD_NAME),
                "UNRESOLVED_RETRY tail lookup owner"
            );
            let sequence = self.tail_lookup_sequence.fetch_add(1, Ordering::SeqCst);
            let expected: &[&str] = if sequence.is_multiple_of(2) {
                &["asset-owned-master"]
            } else {
                &["asset-owned-master", "owned-master"]
            };
            assert_eq!(
                requested_names.as_slice(),
                expected,
                "UNRESOLVED_RETRY source confirmation shape at sequence {sequence}: {requested_names:?}"
            );
            eprintln!(
                "KEI_MIXED_UNRESOLVED_RETRY_LOOKUP {}",
                json!({
                    "zone": self.zone,
                    "cycle": self.cycle,
                    "sequence": sequence,
                    "record_names": requested_names,
                })
            );
        } else {
            assert_eq!(request["zoneID"]["zoneName"], self.zone);
        }
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
            zone_id: Arc::new(zone_scope(provider.zone)),
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
// Supported provider-v1 whole-file signature: 0x01 followed by SHA-1 over the
// two NUL-terminated salts and every byte. Production compares this against
// retained bytes when a receipt has no source hash.
fn provider_signature(bytes: &[u8]) -> String {
    use sha1::{Digest as _, Sha1};
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    data_encoding::BASE64.encode(&[vec![1], hash.finalize().to_vec()].concat())
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

#[derive(Clone, Copy)]
enum SeedSource {
    Unknown,
    Verified,
}

async fn seed_file(
    db: &state::SqliteStateDb,
    zone: usize,
    id: &str,
    version: state::VersionSizeKey,
    target: &Path,
    bytes: &[u8],
    source: SeedSource,
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
    match source {
        SeedSource::Unknown => {
            db.mark_downloaded(
                ZONES[zone],
                id,
                version.as_str(),
                target,
                &hash,
                Some(&hash),
            )
            .await
        }
        SeedSource::Verified => {
            state::DownloadStateStore::mark_verified_download(
                db,
                ZONES[zone],
                id,
                version.as_str(),
                target,
                &hash,
                Some(&hash),
                false,
            )
            .await
        }
    }
    .unwrap();
    let stored: Option<String> = db.acquire_lock("explicit fixture source provenance").unwrap()
        .query_row("SELECT source_checksum FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4", rusqlite::params![ZONES[zone], id, version.as_str(), target.to_string_lossy()], |row| row.get(0)).unwrap();
    assert_eq!(
        stored,
        match source {
            SeedSource::Unknown => None,
            SeedSource::Verified => Some(hash),
        }
    );
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

// Read-only diagnostic tuple for the single authenticated selected-reachability
// case. It observes only the synthetic fixture and never mutates state.
struct DiagnosticContext<'a> {
    replay: Replay,
    database: &'a Path,
    media: &'a Path,
    seeded_paths: &'a BTreeMap<PathBuf, Vec<u8>>,
    requests: &'a [Arc<AtomicUsize>],
    hydrations: &'a [Arc<AtomicUsize>],
    witness: &'a Arc<Mutex<Vec<FaultWitness>>>,
    page_hits: &'a Arc<Mutex<Vec<PageHit>>>,
}

fn rows_json(database: &Path, sql: &str) -> Vec<Vec<Value>> {
    rows(database, sql)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(value) => json!(value),
                    rusqlite::types::Value::Real(value) => json!(value),
                    rusqlite::types::Value::Text(value) => json!(value),
                    rusqlite::types::Value::Blob(value) => json!(local_hash(&value)),
                })
                .collect()
        })
        .collect()
}

fn diagnostic_tuple(
    context: &DiagnosticContext<'_>,
    phase: &str,
    cycle: usize,
    priming: bool,
    result: &crate::sync_cycle::CycleResult,
    config: &crate::config::Config,
) -> Value {
    let files_now = files(context.media);
    let file_rows: Vec<Value> = files_now
        .iter()
        .map(|(path, bytes)| {
            json!({
                "path": path.to_string_lossy(),
                "bytes": bytes.len(),
                "sha256": local_hash(bytes),
            })
        })
        .collect();
    let growth: Vec<Value> = files_now
        .iter()
        .filter(|(path, _)| !context.seeded_paths.contains_key(*path))
        .map(|(path, bytes)| {
            json!({
                "path": path.to_string_lossy(),
                "bytes": bytes.len(),
                "sha256": local_hash(bytes),
            })
        })
        .collect();
    json!({
        "phase": phase,
        "replay": context.replay,
        "cycle": cycle,
        "priming": priming,
        "config": {
            "live_photo_mode": format!("{:?}", config.photos.live_photo_mode),
            "albums": format!("{:?}", config.filters.selection.albums),
            "albums_explicit": config.filters.selection.albums_explicit,
        },
        "stats": {
            "downloaded": result.stats.downloaded,
            "videos_downloaded": result.stats.videos_downloaded,
            "metadata_capture_refreshed": result.stats.metadata_capture_refreshed,
            "sync_token_blocked": result.stats.sync_token_blocked,
            "sync_token_blocked_zone": result.stats.sync_token_blocked_zone,
            "sync_token_blocked_reason": result.stats.sync_token_blocked_reason,
            "full_enumeration_reason": format!("{:?}", result.stats.full_enumeration_reason),
            "state_write_failures": result.stats.state_write_failures,
        },
        "tokens": rows_json(context.database, "SELECT key,value FROM metadata WHERE key LIKE 'sync_token:%' OR key LIKE 'pending_sync_token:%' OR key LIKE 'unresolved_asset_identity:%' ORDER BY key"),
        "enum_hashes": rows_json(context.database, "SELECT key,value FROM metadata WHERE key IN ('enum_config_hash','pending_enum_config_hash') ORDER BY key"),
        "assets": rows_json(context.database, "SELECT library,id,version_size,status,local_path,last_error,download_attempts FROM assets WHERE id IN ('asset-owned-master','legacy-master','current-a','current-b','asset-live-master') ORDER BY 1,2,3"),
        "mappings": rows_json(context.database, "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings ORDER BY 1,2"),
        "album_containers": rows_json(context.database, "SELECT library,container_id,album_name,pass_kind,is_deleted FROM album_containers ORDER BY 1,2"),
        "album_snapshots": rows_json(context.database, "SELECT library,container_id,generation,status,enum_config_hash FROM album_membership_snapshots ORDER BY 1,2,3"),
        "album_memberships": rows_json(context.database, "SELECT library,asset_record_name,master_record_name,container_id,generation,is_deleted,source FROM asset_album_memberships ORDER BY 1,2,4"),
        "paths": rows_json(context.database, "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum FROM asset_metadata_paths WHERE id IN ('asset-owned-master','asset-live-master','current-a','current-b') ORDER BY 1,2,3,4"),
        "receipts": rows_json(context.database, "SELECT library,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,trust_receipt,generation_id FROM download_receipts ORDER BY 1,2,3,4"),
        "provider_generations": rows_json(context.database, "SELECT id,scope,basis,sealed,checkpoint_ready,checkpoint_veto,replay_after FROM provider_active_generations ORDER BY id"),
        "provider_decisions": rows_json(context.database, "SELECT pass_key,child,outcome,admission,reason,attempts,next_retry_at FROM provider_active_decisions ORDER BY 1,2"),
        "provider_destinations": rows_json(context.database, "SELECT pass_key,child,library,asset_id,master,version_size,path,admitted,verified_media,verified_metadata,local_checksum,source_checksum FROM provider_active_destinations ORDER BY 1,2,5,6"),
        "provider_sources": rows_json(context.database, "SELECT pass_key,child,page_id,ordinal FROM provider_active_sources ORDER BY 1,2,3,4"),
        "metadata_retries": rows_json(context.database, "SELECT library,asset_id,evidence,generation,attempts,last_attempt_at,next_retry_at FROM metadata_capture_retries ORDER BY 1,2,3"),
        "sparse_retries": rows_json(context.database, "SELECT library,source_record_name,generation,attempts,next_retry_at,last_outcome FROM unresolved_sparse_identities ORDER BY 1,2"),
        "source_revalidation_debt": rows_json(context.database, "SELECT round,id,next_retry_at FROM source_revalidation_debt ORDER BY 1,2"),
        "files": file_rows,
        "file_growth": growth,
        "request_counts": context.requests.iter().map(|value| value.load(Ordering::SeqCst)).collect::<Vec<_>>(),
        "hydration_counts": context.hydrations.iter().map(|value| value.load(Ordering::SeqCst)).collect::<Vec<_>>(),
        "page_hits": context.page_hits.lock().expect("record page witness lock").clone(),
        "fault_witness": format!("{:?}", context.witness.lock().expect("fault witness lock")),
    })
}

fn emit_diagnostic(
    context: &DiagnosticContext<'_>,
    phase: &str,
    cycle: usize,
    priming: bool,
    result: &crate::sync_cycle::CycleResult,
    config: &crate::config::Config,
) {
    if std::env::var("KEI_MIXED_REPLAY").is_err() {
        return;
    }
    let tuple = diagnostic_tuple(context, phase, cycle, priming, result, config);
    eprintln!(
        "KEI_MIXED_DIAG {}",
        serde_json::to_string(&tuple).expect("diagnostic tuple serialization")
    );
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

fn text(value: &rusqlite::types::Value, what: &str) -> String {
    match value {
        rusqlite::types::Value::Text(value) => value.clone(),
        other => panic!("{what} must be text: {other:?}"),
    }
}

fn integer(value: &rusqlite::types::Value, what: &str) -> i64 {
    match value {
        rusqlite::types::Value::Integer(value) => *value,
        other => panic!("{what} must be an integer: {other:?}"),
    }
}

// Durable provider graph keys that must not grow after cycle 3. Permitted
// replay bookkeeping (`replay_after`, `last_replayed_at`) and per-attempt
// diagnostic timestamps are checked separately; generated IDs and wall times
// are representation only and are not normalized here.
fn provider_graph(database: &Path) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    [
        "SELECT id,scope,config_hash,basis,specification_hash,metadata_enabled,sealed,checkpoint_ready,created_at FROM provider_active_generations ORDER BY id",
        "SELECT generation,pass_key,child,outcome,admission,reason,manifest_hash FROM provider_active_decisions ORDER BY 1,2,3",
        "SELECT generation,pass_key,child,library,asset_id,master,version_size,path,admitted,verified_media,verified_metadata,local_checksum,source_checksum FROM provider_active_destinations ORDER BY 1,2,3,5,6",
        "SELECT generation,pass_key,child,page_id,ordinal FROM provider_active_sources ORDER BY 1,2,3,4",
        "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum FROM asset_metadata_paths ORDER BY 1,2,3,4",
        "SELECT library,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,trust_receipt FROM download_receipts ORDER BY 1,2,3,4",
        "SELECT library,asset_id,evidence,generation,attempts,last_attempt_at,next_retry_at FROM metadata_capture_retries ORDER BY 1,2",
        "SELECT library,source_record_name,generation,attempts,next_retry_at,last_outcome FROM unresolved_sparse_identities ORDER BY 1,2",
        "SELECT round,id,next_retry_at FROM source_revalidation_debt ORDER BY 1,2",
        "SELECT library,container_id,generation,status,enum_config_hash FROM album_membership_snapshots ORDER BY 1,2,3",
        "SELECT library,asset_record_name,master_record_name,container_id,generation,is_deleted,source FROM asset_album_memberships ORDER BY 1,2,4",
    ]
    .iter()
    .map(|sql| rows(database, sql))
    .collect()
}

fn unknown_pass_keys(database: &Path) -> HashSet<String> {
    rows(
        database,
        "SELECT DISTINCT pass_key FROM provider_active_decisions WHERE child='asset-owned-master'",
    )
    .into_iter()
    .map(|row| text(&row[0], "unknown pass key"))
    .collect()
}

fn unknown_generations(database: &Path) -> HashSet<String> {
    rows(database, "SELECT id FROM provider_active_generations")
        .into_iter()
        .map(|row| text(&row[0], "generation id"))
        .collect()
}

// Permitted bookkeeping only: current config hash, format-2 specification and
// profile pass keys; `replay_after` may move within the frozen unknown pass and
// child universe; `last_replayed_at` may advance monotonically. Decision
// attempts and deadlines stay at the known zero state.
fn assert_provider_bookkeeping(
    database: &Path,
    cycle: usize,
    replay: Replay,
    previous_last_replayed: &BTreeMap<String, i64>,
) -> BTreeMap<String, i64> {
    let pass_keys = unknown_pass_keys(database);
    let mut next_last = BTreeMap::new();
    let generations = rows(
        database,
        "SELECT id,json_extract(CAST(specification AS TEXT),'$.format'),json_extract(CAST(specification AS TEXT),'$.profile.passes'),replay_after,last_replayed_at FROM provider_active_generations ORDER BY id",
    );
    assert!(!generations.is_empty(), "provider generations exist");
    let mut profile_keys = HashSet::new();
    for row in generations {
        let id = text(&row[0], "generation id");
        assert_eq!(
            integer(&row[1], "generation spec format"),
            2,
            "generation specification format is 2, not the authentication descriptor {replay:?} cycle={cycle} id={id}"
        );
        let passes: Value = serde_json::from_str(&text(&row[2], "generation profile passes"))
            .expect("generation profile passes JSON");
        for key in passes
            .as_array()
            .expect("generation profile passes array")
            .iter()
            .filter_map(|pass| pass.get("key").and_then(Value::as_str))
        {
            profile_keys.insert(key.to_owned());
        }
        if let rusqlite::types::Value::Text(replay_after) = &row[3] {
            let after: Vec<String> =
                serde_json::from_str(replay_after).expect("replay cursor JSON");
            assert_eq!(after.len(), 2, "replay cursor pass/child shape");
            assert!(
                pass_keys.contains(&after[0]),
                "replay cursor stays within the frozen unknown pass keys {replay:?} cycle={cycle} id={id}: {after:?}"
            );
            assert_eq!(
                after[1], "asset-owned-master",
                "replay cursor stays on the known unknown child {replay:?} cycle={cycle} id={id}"
            );
        }
        let last = integer(&row[4], "last_replayed_at");
        if let Some(previous) = previous_last_replayed.get(&id) {
            assert!(
                last >= *previous,
                "last_replayed_at is monotonic {replay:?} cycle={cycle} id={id}: {last} < {previous}"
            );
        }
        next_last.insert(id, last);
    }
    for key in &pass_keys {
        assert!(
            profile_keys.contains(key),
            "the frozen generation profiles retain unknown pass key {replay:?} cycle={cycle}: {key}"
        );
    }
    let decisions = rows(
        database,
        "SELECT generation,pass_key,child,attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-owned-master' ORDER BY 1,2",
    );
    assert!(!decisions.is_empty(), "known unknown decisions exist");
    for row in decisions {
        assert_eq!(
            integer(&row[3], "decision attempts"),
            0,
            "known unknown decision attempts stay zero {replay:?} cycle={cycle}"
        );
        assert_eq!(
            integer(&row[4], "decision next_retry_at"),
            0,
            "known unknown decision deadline stays zero {replay:?} cycle={cycle}"
        );
    }
    next_last
}

fn retained_refusal_observed(database: &Path) -> BTreeMap<(String, String), i64> {
    let mut observed = BTreeMap::new();
    for row in rows(
        database,
        "SELECT library,version_size,last_error FROM assets WHERE id='asset-owned-master' AND status='pending' ORDER BY 1,2",
    ) {
        let library = text(&row[0], "pending library");
        let version = text(&row[1], "pending version");
        let error = text(&row[2], "pending error");
        let structured = error
            .strip_prefix("existing_match_v1:")
            .expect("existing_match_v1 error kind");
        let parsed: Value = serde_json::from_str(structured).expect("pending error JSON");
        let last = parsed["attempts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|attempt| attempt["observed_at"].as_i64())
            .max()
            .unwrap_or(0);
        observed.insert((library, version), last);
    }
    observed
}

fn assert_retained_refusal_diagnostics(
    database: &Path,
    media: &Path,
    seeded_paths: &BTreeMap<PathBuf, Vec<u8>>,
    cycle: usize,
    replay: Replay,
    previous_observed: &BTreeMap<(String, String), i64>,
) {
    let pending = rows(
        database,
        "SELECT library,version_size,last_error FROM assets WHERE id='asset-owned-master' AND status='pending' ORDER BY 1,2",
    );
    let expected = if matches!(replay.history, History::SelectionChange) {
        4
    } else {
        0
    };
    assert_eq!(
        pending.len(),
        expected,
        "known unknown pending refusal rows {replay:?} cycle={cycle}: {pending:?}"
    );
    if pending.is_empty() {
        return;
    }
    let generations = unknown_generations(database);
    let pass_keys = unknown_pass_keys(database);
    let mut allowed_paths: HashSet<String> = seeded_paths
        .keys()
        .filter(|path| {
            let path = path.to_string_lossy();
            ZONES.iter().any(|name| {
                path.starts_with(&format!("{name}-B/")) || path.starts_with(&format!("{name}-C/"))
            })
        })
        .map(|path| media.join(path).to_string_lossy().into_owned())
        .collect();
    for name in ZONES {
        for label in ["B", "C"] {
            for filename in [
                "photo.JPG",
                "photo.MOV",
                "photo-asset-owned-master-33.JPG",
                "photo-asset-owned-master-33.MOV",
            ] {
                allowed_paths.insert(
                    media
                        .join(format!("{name}-{label}/{filename}"))
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let now = chrono::Utc::now().timestamp();
    let mut next_observed = BTreeMap::new();
    for row in pending {
        let library = text(&row[0], "pending library");
        let version = text(&row[1], "pending version");
        let error = text(&row[2], "pending error");
        let structured = error
            .strip_prefix("existing_match_v1:")
            .unwrap_or_else(|| panic!("pending error kind {replay:?} cycle={cycle}: {error}"));
        let parsed: Value =
            serde_json::from_str(structured).expect("pending error structured JSON");
        let attempts = parsed["attempts"]
            .as_array()
            .expect("pending attempts array");
        assert!(
            attempts.len() <= 16,
            "existing_match diagnostics stay within the 16 cap {replay:?} cycle={cycle} {library}/{version}: {}",
            attempts.len()
        );
        let mut reasons = HashSet::new();
        let mut last_observed = 0_i64;
        for attempt in attempts {
            let generation = attempt["generation"].as_str().expect("attempt generation");
            let pass = attempt["pass"].as_str().expect("attempt pass");
            let child = attempt["child"].as_str().expect("attempt child");
            let observed = attempt["observed_at"]
                .as_i64()
                .expect("attempt observed_at");
            assert!(
                generations.contains(generation),
                "attempt generation stays in the frozen graph {replay:?} cycle={cycle}: {generation}"
            );
            assert!(
                pass_keys.contains(pass),
                "attempt pass stays in the frozen unknown pass keys {replay:?} cycle={cycle}: {pass}"
            );
            assert_eq!(
                child, "asset-owned-master",
                "attempt child stays on the known unknown asset {replay:?} cycle={cycle}"
            );
            assert!(
                observed >= last_observed,
                "attempt observed_at is non-decreasing per key {replay:?} cycle={cycle} {library}/{version}: {observed} < {last_observed}"
            );
            assert!(
                observed <= now,
                "attempt observed_at is not in the future {replay:?} cycle={cycle}: {observed} > {now}"
            );
            last_observed = observed;
            let destination = attempt["destination"]["path"]
                .as_str()
                .expect("attempt destination path");
            assert!(
                allowed_paths.contains(destination),
                "attempt destination stays in the known B/C path universe {replay:?} cycle={cycle}: {destination}"
            );
            for candidate in attempt["explanation"]["candidates"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let path = candidate["path"]["path"].as_str().expect("candidate path");
                assert!(
                    allowed_paths.contains(path),
                    "candidate path stays in the known B/C path universe {replay:?} cycle={cycle}: {path}"
                );
                if let Some(reason) = candidate["reason"].as_str() {
                    reasons.insert(reason.to_owned());
                }
            }
        }
        assert_eq!(
            reasons,
            HashSet::from([
                "no_valid_receipt_or_supported_provider_proof".to_owned(),
                "unsupported_provider_proof_without_valid_receipt".to_owned(),
            ]),
            "exact known refusal reason set {replay:?} cycle={cycle} {library}/{version}"
        );
        let previous = previous_observed
            .get(&(library.clone(), version.clone()))
            .copied()
            .unwrap_or(0);
        assert!(
            last_observed >= previous,
            "last known observed_at is unchanged or advanced {replay:?} cycle={cycle} {library}/{version}: {last_observed} < {previous}"
        );
        next_observed.insert((library, version), last_observed);
    }
    assert_eq!(
        next_observed.len(),
        4,
        "four known refusal diagnostic keys {replay:?} cycle={cycle}"
    );
}

#[derive(Debug)]
struct PrimingEvidence {
    state: Vec<Vec<Vec<rusqlite::types::Value>>>,
    tokens: Vec<(String, Option<String>)>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    media_gets: usize,
    downloaded: usize,
    metadata_capture_refreshed: usize,
    hydrations: usize,
    provider_requests: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct TailProof {
    cycle: usize,
    lookups: [usize; 2],
    visits: [usize; 2],
    changes: [usize; 2],
    readiness: [usize; 2],
    data_queries: [usize; 2],
}

#[derive(Debug)]
struct HistoryEvidence {
    priming: PrimingEvidence,
    outputs: Vec<Vec<Vec<Vec<rusqlite::types::Value>>>>,
    tail_proofs: Vec<TailProof>,
}

fn cycle_labels(history: History, cycle: usize) -> &'static [&'static str] {
    if matches!(history, History::SelectionChange) {
        match cycle {
            0 => &["B"],
            1 => &["C", "B"],
            _ => &["B", "C"],
        }
    } else {
        &["B", "C"]
    }
}

// SelectionChange drives the ImageOnly-to-Both transition through both the
// cycle config and the built download config; only IncompleteInventory keeps
// the default mode, so this history owns the selection drift without adding a
// second unrelated checkpoint reason.
fn cycle_live_photo_mode(history: History, cycle: usize) -> crate::types::LivePhotoMode {
    if matches!(history, History::SelectionChange) && cycle == 0 {
        crate::types::LivePhotoMode::ImageOnly
    } else {
        crate::types::LivePhotoMode::Both
    }
}

fn history_cycle_config(history: History, cycle: usize) -> crate::config::Config {
    let mut config = make_run_cycle_config();
    let labels = cycle_labels(history, cycle);
    config.filters.selection.albums = crate::selection::AlbumSelector::Named {
        included: ZONES
            .iter()
            .flat_map(|zone| labels.iter().map(move |label| format!("{zone}-{label}")))
            .collect(),
        excluded: Default::default(),
    };
    config.filters.selection.albums_explicit = true;
    config.photos.live_photo_mode = cycle_live_photo_mode(history, cycle);
    config
}

struct LibraryBatch {
    replay: Replay,
    cycle: usize,
    priming: bool,
    records_by_zone: Vec<Vec<Value>>,
    start_tokens: Vec<String>,
    requests: Vec<Arc<AtomicUsize>>,
    hydrations: Vec<Arc<AtomicUsize>>,
    tail_changes: Vec<Arc<AtomicUsize>>,
    tail_readiness: Vec<Arc<AtomicUsize>>,
    tail_data_queries: Vec<Arc<AtomicUsize>>,
    tail_lookup_sequence: Vec<Arc<AtomicUsize>>,
    witness: Arc<Mutex<Vec<FaultWitness>>>,
    page_hits: Arc<Mutex<Vec<PageHit>>>,
}

async fn build_history_libraries(
    batch: &LibraryBatch,
    db: &Arc<state::SqliteStateDb>,
) -> Vec<crate::sync_cycle::LibraryState> {
    let mut libraries = Vec::new();
    for (zone, name) in ZONES.iter().enumerate() {
        let mut records = if batch.priming {
            Vec::new()
        } else {
            batch.records_by_zone[zone].clone()
        };
        if !batch.priming
            && zone == 0
            && batch.cycle < 2
            && matches!(batch.replay.history, History::SelectionChange)
        {
            records[2]["fields"]
                .as_object_mut()
                .unwrap()
                .remove("assetDate");
        }
        let unresolved_retry_tail = !batch.priming && batch.cycle >= 4;
        let fault_active = !batch.priming
            && zone == 0
            && batch.cycle < 2
            && matches!(batch.replay.history, History::IncompleteInventory);
        let expected_cursor = (!batch.priming).then(|| batch.start_tokens[zone].clone());
        let capture = ShadowCapture::new(db.clone(), mixed_owner(), "com");
        let library_provider = provider(ProviderInput {
            zone: name,
            query_records: if batch.priming {
                Vec::new()
            } else {
                unfiled_query_records(&records)
            },
            records: records.clone(),
            encoding: batch.replay.encoding,
            unresolved_retry_tail,
            requests: batch.requests[zone].clone(),
            hydrations: batch.hydrations[zone].clone(),
            tail_changes: batch.tail_changes[zone].clone(),
            tail_readiness: batch.tail_readiness[zone].clone(),
            tail_data_queries: batch.tail_data_queries[zone].clone(),
            tail_lookup_sequence: batch.tail_lookup_sequence[zone].clone(),
            priming: batch.priming,
            cycle: batch.cycle,
            expected_cursor: expected_cursor.clone(),
            fault_active,
            witness: batch.witness.clone(),
            page_hits: batch.page_hits.clone(),
        });
        let mut library = PhotoLibrary::new(
            format!("https://synthetic.example/{name}/photos/production/private"),
            Arc::new(std::collections::HashMap::new()),
            Box::new(library_provider),
            Arc::new(zone_scope(name)),
            "private".to_string(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .expect("real photo library");
        library.shadow_capture = Some(capture.clone());
        let mut passes = Vec::new();
        for label in cycle_labels(batch.replay.history, batch.cycle) {
            let mut pass_album = album(
                provider(ProviderInput {
                    zone: name,
                    records: records.clone(),
                    query_records: if batch.priming {
                        Vec::new()
                    } else {
                        album_query_records(&batch.records_by_zone[zone])
                    },
                    encoding: batch.replay.encoding,
                    unresolved_retry_tail,
                    requests: batch.requests[zone].clone(),
                    hydrations: batch.hydrations[zone].clone(),
                    tail_changes: batch.tail_changes[zone].clone(),
                    tail_readiness: batch.tail_readiness[zone].clone(),
                    tail_data_queries: batch.tail_data_queries[zone].clone(),
                    tail_lookup_sequence: batch.tail_lookup_sequence[zone].clone(),
                    priming: batch.priming,
                    cycle: batch.cycle,
                    expected_cursor: expected_cursor.clone(),
                    fault_active,
                    witness: batch.witness.clone(),
                    page_hits: batch.page_hits.clone(),
                }),
                &format!("{name}-{label}"),
                Some(label),
            );
            pass_album.set_shadow_capture(capture.clone(), "private".into());
            // Diagnostic restoration only: the album passes select the
            // unknown-source B/C asset exactly as the original parent did.
            // Unfiled keeps its own original exclusion below.
            passes.push(AlbumPass {
                kind: PassKind::Album,
                album: pass_album,
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            });
        }
        let mut unfiled_album = album(
            provider(ProviderInput {
                zone: name,
                records: records.clone(),
                query_records: if batch.priming {
                    Vec::new()
                } else {
                    unfiled_query_records(&records)
                },
                encoding: batch.replay.encoding,
                unresolved_retry_tail,
                requests: batch.requests[zone].clone(),
                hydrations: batch.hydrations[zone].clone(),
                tail_changes: batch.tail_changes[zone].clone(),
                tail_readiness: batch.tail_readiness[zone].clone(),
                tail_data_queries: batch.tail_data_queries[zone].clone(),
                tail_lookup_sequence: batch.tail_lookup_sequence[zone].clone(),
                priming: batch.priming,
                cycle: batch.cycle,
                expected_cursor,
                fault_active,
                witness: batch.witness.clone(),
                page_hits: batch.page_hits.clone(),
            }),
            "",
            None,
        );
        unfiled_album.set_shadow_capture(capture, "private".into());
        passes.push(AlbumPass {
            kind: PassKind::Unfiled,
            album: unfiled_album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::from_iter([
                "asset-owned-master".to_string()
            ])),
        });
        let mut library_state =
            make_run_cycle_library_state_with_passes(name, &sync_token_key(name), passes);
        library_state.library = library;
        libraries.push(library_state);
    }
    libraries
}

async fn run_history(replay: Replay) -> HistoryEvidence {
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
        let db = state::SqliteStateDb::open_owned(&database, &mixed_owner())
            .await
            .unwrap();
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
            // A genuine verified live publication with one canonical path
            // supplies the rendition transition evidence. It is independent of
            // the unknown-source B/C receipts, which stay the refusal control.
            let mut live_page = full_album_page_with_download(
                name,
                "live-master",
                "after",
                &format!("{}/{zone}.jpg", server.uri()),
                bytes.len() as u64,
                &checksum(&bytes),
            );
            live_page["records"][0]["fields"]["filenameEnc"] =
                json!({"value":"live.JPG","type":"STRING"});
            // The motion file exists at its expected path without a receipt.
            // Only the supported v1 signature can admit it after Both.
            live_page["records"][0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/{zone}.mov",server.uri()),"size":motion.len(),"fileChecksum":provider_signature(&motion)}});
            live_page["records"][0]["fields"]["resOriginalVidComplFileType"] =
                json!({"value":"com.apple.quicktime-movie"});
            live_page["records"][1]["recordName"] = json!("asset-live-master");
            live_page["records"][1]["fields"]["assetDate"]["value"] =
                json!(date(zone).timestamp_millis());
            live_page["records"][1]["fields"]["addedDate"]["value"] =
                json!(date(zone).timestamp_millis());
            records.extend(live_page["records"].as_array().unwrap().clone());
            let legacy = media.join(date_dir(zone)).join("photo.JPG");
            seed_file(
                &db,
                zone,
                "legacy-master",
                state::VersionSizeKey::Original,
                &legacy,
                &bytes,
                SeedSource::Unknown,
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
            // A current child has verified source before this history starts;
            // the legacy row and a later sibling must never absorb its receipt.
            let current = media.join(date_dir(zone)).join("photo-current-a-9.JPG");
            seed_file(
                &db,
                zone,
                "current-a",
                state::VersionSizeKey::Original,
                &current,
                &bytes,
                SeedSource::Verified,
            )
            .await;
            let live_still = media.join(date_dir(zone)).join("live.JPG");
            seed_file(
                &db,
                zone,
                "asset-live-master",
                state::VersionSizeKey::Original,
                &live_still,
                &bytes,
                SeedSource::Verified,
            )
            .await;
            // The motion bytes are present but unreceipted before Both.
            std::fs::write(media.join(date_dir(zone)).join("live.MOV"), &motion).unwrap();
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
            // Membership predates these completed publication receipts, which
            // keep unknown source exactly as generic historical receipts do.
            // Selecting the other album must refuse reuse without promoting
            // that source or manufacturing a new duplicate.
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
                    seed_file(
                        &db,
                        zone,
                        "asset-owned-master",
                        version,
                        &target,
                        content,
                        SeedSource::Unknown,
                    )
                    .await;
                }
            }
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
    let tail_changes: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let tail_readiness: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let tail_data_queries: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let tail_lookup_sequence: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let witness: Arc<Mutex<Vec<FaultWitness>>> = Arc::new(Mutex::new(Vec::new()));
    let page_hits: Arc<Mutex<Vec<PageHit>>> = Arc::new(Mutex::new(Vec::new()));
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;

    // One real production full cycle establishes the current predecessor. The
    // empty inventory admits no media; the /zones/list boundary is bridged to
    // mixed-{zone}-c0. Historical rows, retries and seeded bytes are untouched.
    let priming = {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&database, &mixed_owner())
                .await
                .unwrap(),
        );
        let libraries = build_history_libraries(
            &LibraryBatch {
                replay,
                cycle: 0,
                priming: true,
                records_by_zone: records_by_zone.clone(),
                start_tokens: Vec::new(),
                requests: requests.clone(),
                hydrations: hydrations.clone(),
                tail_changes: tail_changes.clone(),
                tail_readiness: tail_readiness.clone(),
                tail_data_queries: tail_data_queries.clone(),
                tail_lookup_sequence: tail_lookup_sequence.clone(),
                witness: witness.clone(),
                page_hits: page_hits.clone(),
            },
            &db,
        )
        .await;
        let config = history_cycle_config(replay.history, 0);
        let rendition_mode = cycle_live_photo_mode(replay.history, 0);
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
            Arc::make_mut(&mut built).live_photo_mode = rendition_mode;
            built
        };
        let result = crate::sync_cycle::run_cycle(
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
        .unwrap();
        drop(base_build);
        drop(libraries);
        drop(db);
        emit_diagnostic(
            &DiagnosticContext {
                replay,
                database: &database,
                media: &media,
                seeded_paths: &seeded_paths,
                requests: &requests,
                hydrations: &hydrations,
                witness: &witness,
                page_hits: &page_hits,
            },
            "priming",
            0,
            true,
            &result,
            &config,
        );

        let tokens: Vec<_> = {
            let reopened = state::SqliteStateDb::open_owned(&database, &mixed_owner())
                .await
                .unwrap();
            let mut tokens = Vec::new();
            for name in ZONES {
                let key = sync_token_key(name);
                tokens.push((key.clone(), reopened.get_metadata(&key).await.unwrap()));
            }
            tokens
        };
        assert_eq!(
            tokens
                .iter()
                .map(|(_, token)| token.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some(zone_token(ZONES[0], 0).as_str()),
                Some(zone_token(ZONES[1], 0).as_str())
            ],
            "production priming commits the predecessor for every zone"
        );
        assert_eq!(result.stats.downloaded, 0, "priming downloads no media");
        assert_eq!(
            result.stats.metadata_capture_refreshed, 0,
            "priming repairs no metadata"
        );
        assert!(
            !result.stats.sync_token_blocked,
            "priming completes without a checkpoint hold"
        );
        assert_eq!(
            hydrations
                .iter()
                .map(|v| v.load(Ordering::SeqCst))
                .sum::<usize>(),
            0,
            "priming performs no hydration"
        );
        let media_gets = server.received_requests().await.unwrap().len();
        assert_eq!(media_gets, 0, "priming performs no media GET");
        assert_eq!(files(&media), seeded_paths, "priming writes no media");
        assert_eq!(
            rows(
                &database,
                "SELECT library,id,version_size,checksum,created_at,added_at,status,local_path FROM assets WHERE id='legacy-master' ORDER BY library"
            ),
            legacy_rows,
            "priming leaves the historical receipt unchanged"
        );
        assert_eq!(
            rows(
                &database,
                "SELECT library,asset_id FROM metadata_capture_retries ORDER BY library"
            ),
            legacy_retries,
            "priming leaves historical retry evidence unchanged"
        );
        assert!(
            rows(
                &database,
                "SELECT key FROM metadata WHERE key LIKE 'pending_sync_token:%' OR key LIKE 'baseline_cursor:%'"
            )
            .is_empty(),
            "priming consumes its baseline and stages no pending successor"
        );
        let provider_requests = requests.iter().map(|v| v.load(Ordering::SeqCst)).sum();
        assert!(
            provider_requests > 0,
            "priming runs real provider work through the production cycle"
        );
        PrimingEvidence {
            state: snapshot(&database),
            tokens,
            files: files(&media),
            media_gets,
            downloaded: result.stats.downloaded,
            metadata_capture_refreshed: result.stats.metadata_capture_refreshed,
            hydrations: 0,
            provider_requests,
        }
    };

    let mut outputs = Vec::new();
    let mut stable_files = None;
    let mut stable_downloads = 0;
    let mut tail_proofs = Vec::new();
    let mut frozen_provider_graph = None;
    let mut previous_last_replayed = BTreeMap::new();
    let mut previous_observed = BTreeMap::new();
    let mut child_paths = BTreeMap::new();
    let mut healthy_tokens = Vec::new();
    for cycle in 0..CYCLES {
        eprintln!("{replay:?} cycle={cycle}/{CYCLES}");
        witness.lock().expect("fault witness lock").clear();
        page_hits.lock().expect("record page witness lock").clear();
        let tail_start_hydrations: [usize; 2] = hydrations
            .iter()
            .map(|value| value.load(Ordering::SeqCst))
            .collect::<Vec<_>>()
            .try_into()
            .expect("two zone hydration counters");
        if cycle >= 4 {
            for zone in 0..ZONES.len() {
                tail_changes[zone].store(0, Ordering::SeqCst);
                tail_readiness[zone].store(0, Ordering::SeqCst);
                tail_data_queries[zone].store(0, Ordering::SeqCst);
                tail_lookup_sequence[zone].store(0, Ordering::SeqCst);
            }
        }
        let result = {
            let db = Arc::new(
                state::SqliteStateDb::open_owned(&database, &mixed_owner())
                    .await
                    .unwrap(),
            );
            let start_tokens: Vec<String> = {
                let mut tokens = Vec::new();
                for name in ZONES {
                    tokens.push(
                        db.get_metadata(&sync_token_key(name))
                            .await
                            .unwrap()
                            .expect("priming established every zone predecessor"),
                    );
                }
                tokens
            };
            for zone in 0..ZONES.len() {
                assert_eq!(
                    start_tokens[zone],
                    start_token(replay.history, zone, cycle),
                    "cycle start cursor {replay:?} cycle={cycle} zone={}",
                    ZONES[zone]
                );
            }
            let libraries = build_history_libraries(
                &LibraryBatch {
                    replay,
                    cycle,
                    priming: false,
                    records_by_zone: records_by_zone.clone(),
                    start_tokens: start_tokens.clone(),
                    requests: requests.clone(),
                    hydrations: hydrations.clone(),
                    tail_changes: tail_changes.clone(),
                    tail_readiness: tail_readiness.clone(),
                    tail_data_queries: tail_data_queries.clone(),
                    tail_lookup_sequence: tail_lookup_sequence.clone(),
                    witness: witness.clone(),
                    page_hits: page_hits.clone(),
                },
                &db,
            )
            .await;
            let config = history_cycle_config(replay.history, cycle);
            let rendition_mode = cycle_live_photo_mode(replay.history, cycle);
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
                Arc::make_mut(&mut built).live_photo_mode = rendition_mode;
                built
            };
            let result = crate::sync_cycle::run_cycle(
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
            .unwrap();
            emit_diagnostic(
                &DiagnosticContext {
                    replay,
                    database: &database,
                    media: &media,
                    seeded_paths: &seeded_paths,
                    requests: &requests,
                    hydrations: &hydrations,
                    witness: &witness,
                    page_hits: &page_hits,
                },
                "cycle",
                cycle,
                false,
                &result,
                &config,
            );
            drop(libraries);
            (result, start_tokens)
        };
        let (result, start_tokens) = result;
        let served_pages = page_hits.lock().expect("record page witness lock").clone();
        if cycle >= 4 {
            assert!(
                served_pages.is_empty(),
                "UNRESOLVED_RETRY tail serves no DATA record pages: {served_pages:?}"
            );
        } else {
            let expected_pages =
                expected_record_page_sizes(replay.encoding, records_by_zone[0].len());
            for (zone, name) in ZONES.iter().enumerate() {
                let actual: Vec<usize> = served_pages
                    .iter()
                    .filter(|(hit_zone, hit_cycle, _)| *hit_zone == *name && *hit_cycle == cycle)
                    .map(|(_, _, size)| *size)
                    .collect();
                let expected = if matches!(replay.history, History::SelectionChange)
                    && cycle == 1
                    && zone == 0
                {
                    // The first drifted full pass rejects the malformed
                    // current child before any changes bridge runs.
                    Vec::new()
                } else {
                    expected_pages.clone()
                };
                assert_eq!(
                    actual, expected,
                    "record pages consumed {replay:?} cycle={cycle} zone={name}"
                );
            }
        }
        // All production handles are gone. Reopen independently after EVERY
        // cycle, including both UNRESOLVED_RETRY tails and the final comparison.
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
        let reopened = state::SqliteStateDb::open_owned(&database, &mixed_owner())
            .await
            .unwrap();
        let downloaded = reopened.get_downloaded_page(0, 100).await.unwrap();
        let current_enum_hash =
            crate::download::compute_config_hash(&history_cycle_config(replay.history, cycle));
        let both_enum_hash = crate::download::compute_config_hash(&history_cycle_config(
            History::SelectionChange,
            1,
        ));
        let image_only_enum_hash = crate::download::compute_config_hash(&history_cycle_config(
            History::SelectionChange,
            0,
        ));
        let expected_enum_hash = match (replay.history, cycle) {
            (History::SelectionChange, 0 | 1) => image_only_enum_hash.clone(),
            (History::SelectionChange, _) => both_enum_hash.clone(),
            (History::IncompleteInventory, _) => current_enum_hash.clone(),
        };
        let expected_pending_enum = (matches!(replay.history, History::SelectionChange)
            && cycle == 1)
            .then(|| both_enum_hash.clone());
        assert_eq!(
            reopened
                .get_metadata("enum_config_hash")
                .await
                .unwrap()
                .as_deref(),
            Some(expected_enum_hash.as_str()),
            "active enumeration config {replay:?} cycle={cycle}"
        );
        assert_eq!(
            reopened
                .get_metadata("pending_enum_config_hash")
                .await
                .unwrap(),
            expected_pending_enum,
            "staged enumeration config {replay:?} cycle={cycle}"
        );
        let pending_tokens = rows(
            &database,
            "SELECT key FROM metadata WHERE key LIKE 'pending_sync_token:%' ORDER BY key",
        );
        if matches!(replay.history, History::SelectionChange) && cycle == 1 {
            assert_eq!(
                pending_tokens.len(),
                1,
                "the healthy peer stages one pending token while PrimarySync's malformed pass is incomplete"
            );
            assert!(
                pending_tokens[0][0]
                    == rusqlite::types::Value::Text(format!(
                        "pending_sync_token:v27:{both_enum_hash}:{}",
                        ZONES[1]
                    )),
                "pending token scope: {pending_tokens:?}"
            );
        } else {
            assert!(
                pending_tokens.is_empty(),
                "no pending token outside the staged reconciliation: {pending_tokens:?}"
            );
        }
        let expected_full = (matches!(replay.history, History::SelectionChange)
            && (cycle == 1 || cycle == 2))
            .then_some(crate::download::FullEnumerationReason::EnumConfigHashDrift);
        assert_eq!(
            result.stats.full_enumeration_reason, expected_full,
            "real config drift chooses full enumeration {replay:?} cycle={cycle}"
        );
        assert_eq!(
            result.stats.state_write_failures, 0,
            "config staging and promotion stay durable {replay:?} cycle={cycle}"
        );
        let mut expected_files = seeded_paths.clone();
        let mut receipt_paths = HashSet::new();
        for (zone, name) in ZONES.iter().enumerate() {
            let actual = reopened.get_metadata(&sync_token_key(name)).await.unwrap();
            assert_eq!(
                actual.as_deref(),
                Some(durable_token_after(replay.history, zone, cycle).as_str()),
                "zone checkpoint {replay:?} cycle={cycle} zone={name}: {:?}",
                result.stats
            );
            if zone == 1 && cycle <= 3 {
                healthy_tokens.push(actual.expect("healthy peer token"));
            }
            let current_b_expected = if zone == 1 {
                true
            } else {
                match replay.history {
                    History::IncompleteInventory => true,
                    History::SelectionChange => cycle >= 2,
                }
            };
            let mut paths = HashSet::new();
            for child in ["current-a", "current-b"] {
                let found = downloaded
                    .iter()
                    .find(|r| r.library.as_ref() == *name && r.id.as_ref() == child);
                let expected_child = child == "current-a" || current_b_expected;
                if expected_child {
                    let row = found.unwrap_or_else(|| {
                        panic!(
                            "current receipt must exist: {replay:?} cycle={cycle} zone={name} child={child}"
                        )
                    });
                    assert_eq!(row.created_at, date(zone));
                    let target = row.local_path.as_ref().unwrap();
                    assert!(
                        paths.insert(target.clone()),
                        "siblings cannot share an owned path"
                    );
                    assert!(
                        receipt_paths.insert(target.clone()),
                        "current receipts cannot share one path across zones"
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
                    expected_files.insert(
                        target.strip_prefix(&media).unwrap().to_path_buf(),
                        bytes_by_zone[zone].0.clone(),
                    );
                } else {
                    assert!(
                        found.is_none(),
                        "malformed current-b cannot create a child receipt"
                    );
                }
            }
        }
        assert_eq!(
            files(&media),
            expected_files,
            "exact owned files from seeds plus recorded current receipts; no duplicate or stale media"
        );
        let leaked = rows(
            &database,
            "SELECT library,id,version_size,status FROM assets WHERE status IN ('pending','failed','source_deleted') ORDER BY 1,2,3",
        );
        if matches!(replay.history, History::SelectionChange) {
            // Exact known refusal contract for the selected unknown-source
            // asset. Cycle 0 proves the asset is still filtered before Both.
            // From cycle 1 on the authenticated B/C album passes admit it and
            // production retains exactly four pending refusal rows.
            let mut expected_pending = Vec::new();
            if cycle >= 1 {
                for name in ZONES {
                    for version in ["live_original", "original"] {
                        expected_pending.push(vec![
                            rusqlite::types::Value::Text(name.to_string()),
                            rusqlite::types::Value::Text("asset-owned-master".to_string()),
                            rusqlite::types::Value::Text(version.to_string()),
                            rusqlite::types::Value::Text("pending".to_string()),
                        ]);
                    }
                }
            }
            assert_eq!(
                leaked, expected_pending,
                "exact known retained refusal pending set for selected asset-owned-master {replay:?} cycle={cycle}: {leaked:?}"
            );
            if cycle >= 1 {
                let refusal_errors = rows(
                    &database,
                    "SELECT library,version_size,last_error FROM assets WHERE id='asset-owned-master' AND status='pending' ORDER BY 1,2",
                );
                assert_eq!(
                    refusal_errors.len(),
                    4,
                    "four retained refusal rows keep one error each {replay:?} cycle={cycle}: {refusal_errors:?}"
                );
                for row in &refusal_errors {
                    let error = match &row[2] {
                        rusqlite::types::Value::Text(value) => value,
                        other => panic!("asset-owned-master last_error must be text: {other:?}"),
                    };
                    let structured = error
                        .strip_prefix("existing_match_v1:")
                        .unwrap_or_else(|| panic!("retained refusal error kind: {error}"));
                    let parsed: Value = serde_json::from_str(structured).unwrap_or_else(|err| {
                        panic!("retained refusal error JSON: {err}: {structured}")
                    });
                    let reasons: Vec<&str> = parsed["attempts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|attempt| {
                            attempt["explanation"]["candidates"]
                                .as_array()
                                .into_iter()
                                .flatten()
                        })
                        .filter_map(|candidate| candidate["reason"].as_str())
                        .collect();
                    for expected_reason in [
                        "no_valid_receipt_or_supported_provider_proof",
                        "unsupported_provider_proof_without_valid_receipt",
                    ] {
                        assert!(
                            reasons.contains(&expected_reason),
                            "retained refusal error carries {expected_reason} {replay:?} cycle={cycle}: {reasons:?}"
                        );
                    }
                }
                let non_null_source = rows(
                    &database,
                    "SELECT library,version_size,local_path,source_checksum FROM asset_metadata_paths WHERE id='asset-owned-master' AND source_checksum IS NOT NULL ORDER BY 1,2,3",
                );
                assert!(
                    non_null_source.is_empty(),
                    "retained refusal keeps source NULL {replay:?} cycle={cycle}: {non_null_source:?}"
                );
                let scoped_receipts = rows(
                    &database,
                    "SELECT library,child,version_size,native_path FROM download_receipts WHERE child='asset-owned-master' ORDER BY 1,2,3,4",
                );
                assert!(
                    scoped_receipts.is_empty(),
                    "retained refusal invents no download receipt {replay:?} cycle={cycle}: {scoped_receipts:?}"
                );
            }
        } else {
            assert!(
                leaked.is_empty(),
                "no erased siblings or leaked transfers: {leaked:?}"
            );
        }
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
        let owned_receipts = rows(
            &database,
            "SELECT library,version_size,provider_checksum,local_checksum,source_checksum,count(*) FROM asset_metadata_paths WHERE id='asset-owned-master' GROUP BY 1,2,3,4,5 ORDER BY 1,2",
        );
        for (zone, name) in ZONES.iter().enumerate() {
            for (version, content) in [
                (state::VersionSizeKey::LiveOriginal, &bytes_by_zone[zone].1),
                (state::VersionSizeKey::Original, &bytes_by_zone[zone].0),
            ] {
                let expected = vec![
                    rusqlite::types::Value::Text(name.to_string()),
                    rusqlite::types::Value::Text(version.as_str().to_string()),
                    rusqlite::types::Value::Text(checksum(content)),
                    rusqlite::types::Value::Text(local_hash(content)),
                    rusqlite::types::Value::Null,
                    rusqlite::types::Value::Integer(2),
                ];
                assert!(
                    owned_receipts.contains(&expected),
                    "unknown-source B/C receipt keeps no source proof {replay:?} cycle={cycle} zone={name}: {owned_receipts:?}"
                );
            }
        }
        let live_receipts = rows(
            &database,
            "SELECT library,version_size,provider_checksum,local_checksum,source_checksum,local_path,count(*) FROM asset_metadata_paths WHERE id='asset-live-master' AND version_size='live_original' GROUP BY 1,2,3,4,5,6 ORDER BY 1",
        );
        let live_expected = !(matches!(replay.history, History::SelectionChange) && cycle == 0);
        if live_expected {
            for (zone, name) in ZONES.iter().enumerate() {
                let motion = &bytes_by_zone[zone].1;
                let expected = vec![
                    rusqlite::types::Value::Text(name.to_string()),
                    rusqlite::types::Value::Text("live_original".to_string()),
                    rusqlite::types::Value::Text(provider_signature(motion)),
                    rusqlite::types::Value::Text(local_hash(motion)),
                    rusqlite::types::Value::Text(local_hash(motion)),
                    rusqlite::types::Value::Text(
                        media
                            .join(date_dir(zone))
                            .join("live.MOV")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    rusqlite::types::Value::Integer(1),
                ];
                assert!(
                    live_receipts.contains(&expected),
                    "Both admits the pre-existing motion through provider proof {replay:?} cycle={cycle} zone={name}: {live_receipts:?}"
                );
            }
        } else {
            assert!(
                live_receipts.is_empty(),
                "ImageOnly leaves the motion unadmitted: {live_receipts:?}"
            );
        }
        assert_eq!(
            result.stats.videos_downloaded, 0,
            "the motion is admitted from existing bytes, never transferred"
        );
        let hits = witness.lock().expect("fault witness lock").clone();
        let expected_hits = if cycle < 2 && matches!(replay.history, History::IncompleteInventory) {
            vec![FaultWitness {
                zone: ZONES[0],
                cycle,
                incoming_cursor: terminal_cursor(ZONES[0], cycle),
                page_role: PageRole::Terminal,
                fault_kind: FaultKind::MissingSuccessor,
            }]
        } else {
            Vec::new()
        };
        assert_eq!(
            hits, expected_hits,
            "exact terminal fault witness on the current delta {replay:?} cycle={cycle}"
        );
        if let Some(reason) = expected_block_reason(replay.history, cycle) {
            assert!(
                result.stats.sync_token_blocked,
                "held token {replay:?}: {:?}",
                result.stats
            );
            assert_eq!(
                result.stats.sync_token_blocked_zone.as_deref(),
                Some(ZONES[0]),
                "only PrimarySync holds"
            );
            assert_eq!(
                result.stats.sync_token_blocked_reason,
                Some(reason),
                "current-source checkpoint hold reason {replay:?} cycle={cycle}"
            );
            assert_eq!(
                start_tokens[0],
                durable_token_after(replay.history, 0, cycle),
                "the held zone keeps its pre-cycle token"
            );
        } else {
            assert!(
                !result.stats.sync_token_blocked,
                "recovery removes only the current fault {replay:?} cycle={cycle}: {:?}",
                result.stats
            );
            assert_eq!(result.stats.sync_token_blocked_reason, None);
        }
        let expected_downloaded = expected_downloads(replay.history, cycle);
        assert_eq!(
            result.stats.downloaded, expected_downloaded,
            "exact first publication work {replay:?} cycle={cycle}"
        );
        let state = snapshot(&database);
        if cycle >= 4 {
            let lookups: [usize; 2] = hydrations
                .iter()
                .enumerate()
                .map(|(zone, value)| value.load(Ordering::SeqCst) - tail_start_hydrations[zone])
                .collect::<Vec<_>>()
                .try_into()
                .expect("two zone lookup deltas");
            let changes: [usize; 2] = tail_changes
                .iter()
                .map(|value| value.load(Ordering::SeqCst))
                .collect::<Vec<_>>()
                .try_into()
                .expect("two zone changes counters");
            let readiness: [usize; 2] = tail_readiness
                .iter()
                .map(|value| value.load(Ordering::SeqCst))
                .collect::<Vec<_>>()
                .try_into()
                .expect("two zone readiness counters");
            let data_queries: [usize; 2] = tail_data_queries
                .iter()
                .map(|value| value.load(Ordering::SeqCst))
                .collect::<Vec<_>>()
                .try_into()
                .expect("two zone data query counters");
            for zone in 0..ZONES.len() {
                assert_eq!(
                    data_queries[zone], 0,
                    "UNRESOLVED_RETRY tail serves no DATA record pages {replay:?} cycle={cycle} zone={}",
                    ZONES[zone]
                );
                assert_eq!(
                    changes[zone], UNRESOLVED_RETRY_EMPTY_DELTAS_PER_ZONE,
                    "UNRESOLVED_RETRY tail keeps one empty delta {replay:?} cycle={cycle} zone={}",
                    ZONES[zone]
                );
                assert_eq!(
                    readiness[zone], UNRESOLVED_RETRY_READINESS_QUERIES_PER_ZONE,
                    "UNRESOLVED_RETRY tail keeps the original readiness protocol {replay:?} cycle={cycle} zone={}",
                    ZONES[zone]
                );
                assert_eq!(
                    lookups[zone] % LOOKUPS_PER_VISIT,
                    0,
                    "UNRESOLVED_RETRY lookups must be whole source-confirmed visits {replay:?} cycle={cycle} zone={}: {}",
                    ZONES[zone],
                    lookups[zone]
                );
                let visits = lookups[zone] / LOOKUPS_PER_VISIT;
                assert!(
                    visits <= VISITS_PER_ZONE,
                    "UNRESOLVED_RETRY derived visit cap {replay:?} cycle={cycle} zone={}: {visits} > {}",
                    ZONES[zone],
                    VISITS_PER_ZONE
                );
                assert!(
                    lookups[zone] <= UNRESOLVED_RETRY_LOOKUP_CAP_PER_ZONE,
                    "UNRESOLVED_RETRY derived lookup cap {replay:?} cycle={cycle} zone={}: {} > {}",
                    ZONES[zone],
                    lookups[zone],
                    UNRESOLVED_RETRY_LOOKUP_CAP_PER_ZONE
                );
            }
            let total: usize = lookups.iter().sum();
            assert!(
                total <= UNRESOLVED_RETRY_LOOKUP_CAP,
                "UNRESOLVED_RETRY derived total lookup cap {replay:?} cycle={cycle}: {total} > {}",
                UNRESOLVED_RETRY_LOOKUP_CAP
            );
            if matches!(replay.history, History::SelectionChange) {
                assert!(
                    total > 0,
                    "the known unresolved retry debt must be observed in both tails {replay:?} cycle={cycle}: {lookups:?}"
                );
            } else {
                assert_eq!(
                    total, 0,
                    "ordinary quiet history has no unresolved retry lookup {replay:?} cycle={cycle}: {lookups:?}"
                );
            }
            let proof = TailProof {
                cycle,
                lookups,
                visits: lookups.map(|count| count / LOOKUPS_PER_VISIT),
                changes,
                readiness,
                data_queries,
            };
            eprintln!(
                "KEI_MIXED_UNRESOLVED_RETRY_TAIL {}",
                serde_json::to_string(&json!({
                    "cycle": cycle,
                    "history": replay.history,
                    "encoding": replay.encoding,
                    "lookups": proof.lookups,
                    "visits": proof.visits,
                    "cap_per_zone": UNRESOLVED_RETRY_LOOKUP_CAP_PER_ZONE,
                    "cap_total": UNRESOLVED_RETRY_LOOKUP_CAP,
                    "changes": proof.changes,
                    "readiness": proof.readiness,
                    "data_queries": proof.data_queries,
                }))
                .expect("tail proof serialization")
            );
            tail_proofs.push(proof);
            assert_eq!(
                result.stats.downloaded, 0,
                "UNRESOLVED_RETRY tail download count"
            );
            assert_eq!(
                result.stats.videos_downloaded, 0,
                "UNRESOLVED_RETRY tail video count"
            );
            assert_eq!(
                result.stats.exif_failures, 0,
                "UNRESOLVED_RETRY tail exif failures"
            );
            assert_eq!(
                result.stats.metadata_capture_refreshed, 0,
                "UNRESOLVED_RETRY tail metadata repair count"
            );
            assert_eq!(
                result.stats.state_write_failures, 0,
                "UNRESOLVED_RETRY tail state write failures"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                stable_downloads,
                "UNRESOLVED_RETRY tail HTTP downloads"
            );
            assert_eq!(
                files(&media),
                *stable_files.as_ref().unwrap(),
                "UNRESOLVED_RETRY tail file growth/bytes"
            );
            assert_eq!(state, outputs[3], "UNRESOLVED_RETRY tail durable state");
        }
        if cycle == 3 {
            stable_files = Some(files(&media));
            stable_downloads = server.received_requests().await.unwrap().len();
            frozen_provider_graph = Some(provider_graph(&database));
            previous_last_replayed =
                assert_provider_bookkeeping(&database, cycle, replay, &BTreeMap::new());
            previous_observed = retained_refusal_observed(&database);
        }
        if cycle >= 4 {
            assert_eq!(
                provider_graph(&database),
                *frozen_provider_graph
                    .as_ref()
                    .expect("cycle-3 provider graph"),
                "frozen provider graph keys/provenance/created_at {replay:?} cycle={cycle}"
            );
            let next_last_replayed =
                assert_provider_bookkeeping(&database, cycle, replay, &previous_last_replayed);
            let replayed: Vec<Value> = next_last_replayed
                .iter()
                .filter(|(id, last)| previous_last_replayed.get(id.as_str()) != Some(*last))
                .map(|(id, last)| json!({"generation": id, "last_replayed_at": last}))
                .collect();
            assert_retained_refusal_diagnostics(
                &database,
                &media,
                &seeded_paths,
                cycle,
                replay,
                &previous_observed,
            );
            let next_observed = retained_refusal_observed(&database);
            let observed: Vec<Value> = next_observed
                .iter()
                .filter(|(key, last)| previous_observed.get(*key) != Some(*last))
                .map(|((library, version), last)| {
                    json!({"library": library, "version": version, "last_observed_at": last})
                })
                .collect();
            eprintln!(
                "KEI_MIXED_UNRESOLVED_RETRY_BOOKKEEPING {}",
                serde_json::to_string(&json!({
                    "cycle": cycle,
                    "replayed_generations": replayed,
                    "observed_at_updates": observed,
                }))
                .expect("bookkeeping delta serialization")
            );
            previous_last_replayed = next_last_replayed;
            previous_observed = next_observed;
        }
        outputs.push(state);
    }
    assert_eq!(
        tail_proofs.len(),
        2,
        "both owned SQLite-reopened UNRESOLVED_RETRY tails executed {replay:?}: {tail_proofs:?}"
    );
    let expected_healthy: Vec<String> = match replay.history {
        History::IncompleteInventory => [1, 2, 3, 4]
            .into_iter()
            .map(|cycle| zone_token(ZONES[1], cycle))
            .collect(),
        History::SelectionChange => [1, 1, 3, 4]
            .into_iter()
            .map(|cycle| zone_token(ZONES[1], cycle))
            .collect(),
    };
    assert_eq!(
        healthy_tokens, expected_healthy,
        "healthy peer progression through real config staging and promotion"
    );
    let mut media_gets: BTreeMap<String, usize> = BTreeMap::new();
    for request in server.received_requests().await.unwrap() {
        *media_gets
            .entry(request.url.path().to_string())
            .or_default() += 1;
    }
    let expected_gets: BTreeMap<String, usize> =
        [(format!("/{}.jpg", 0), 1), (format!("/{}.jpg", 1), 1)]
            .into_iter()
            .collect();
    assert_eq!(
        media_gets, expected_gets,
        "exact logical media GETs per zone and rendition; page shape must not change media work"
    );
    HistoryEvidence {
        priming,
        outputs,
        tail_proofs,
    }
}

fn assert_equivalent(expected: &HistoryEvidence, actual: &HistoryEvidence, context: &str) {
    // Compare durable progress and priming evidence, never raw request counts.
    assert_eq!(
        actual.priming.state, expected.priming.state,
        "priming durable snapshot {context}"
    );
    assert_eq!(
        actual.priming.tokens, expected.priming.tokens,
        "priming token keys {context}"
    );
    assert_eq!(
        actual.priming.files, expected.priming.files,
        "priming file set {context}"
    );
    assert_eq!(
        actual.priming.media_gets, expected.priming.media_gets,
        "priming media work {context}"
    );
    assert_eq!(
        actual.priming.downloaded, expected.priming.downloaded,
        "priming downloads {context}"
    );
    assert_eq!(
        actual.priming.metadata_capture_refreshed, expected.priming.metadata_capture_refreshed,
        "priming metadata work {context}"
    );
    assert_eq!(
        actual.priming.hydrations, expected.priming.hydrations,
        "priming hydrations {context}"
    );
    assert!(
        actual.priming.provider_requests > 0 && expected.priming.provider_requests > 0,
        "priming provider work is reported separately, not compared as request counts {context}"
    );
    assert_eq!(
        actual.outputs, expected.outputs,
        "full durable provider-shape equivalence {context}"
    );
    assert_eq!(
        actual.tail_proofs, expected.tail_proofs,
        "UNRESOLVED_RETRY tail work equivalence {context}"
    );
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
            assert_equivalent(&baseline, &actual, &format!("on replay {replay:?}"));
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
            assert_equivalent(&baseline, &actual, &format!("{history:?} {encoding:?}"));
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
                    SeedSource::Unknown,
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
                unresolved_retry_tail: false,
                requests: Arc::new(AtomicUsize::new(0)),
                hydrations: Arc::new(AtomicUsize::new(0)),
                tail_changes: Arc::new(AtomicUsize::new(0)),
                tail_readiness: Arc::new(AtomicUsize::new(0)),
                tail_data_queries: Arc::new(AtomicUsize::new(0)),
                tail_lookup_sequence: Arc::new(AtomicUsize::new(0)),
                priming: false,
                cycle: 0,
                expected_cursor: None,
                fault_active: false,
                next_expected: Arc::new(Mutex::new(None)),
                next_page: Arc::new(Mutex::new(0)),
                witness: Arc::new(Mutex::new(Vec::new())),
                page_hits: Arc::new(Mutex::new(Vec::new())),
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
        let receipts = rows(
            &database,
            "SELECT version_size,provider_checksum,local_checksum,source_checksum,count(*) FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-owned-master' GROUP BY 1,2,3,4 ORDER BY 1",
        );
        for (version, content) in [
            (state::VersionSizeKey::LiveOriginal, MOTION),
            (state::VersionSizeKey::Original, STILL),
        ] {
            assert!(
                receipts.contains(&vec![
                    rusqlite::types::Value::Text(version.as_str().to_string()),
                    rusqlite::types::Value::Text(checksum(content)),
                    rusqlite::types::Value::Text(local_hash(content)),
                    rusqlite::types::Value::Null,
                    rusqlite::types::Value::Integer(2),
                ]),
                "unknown-source B/C receipts refuse reuse without promotion or duplication: {receipts:?}"
            );
        }
        assert!(
            rows(&database, "SELECT * FROM legacy_master_state_owners").is_empty(),
            "unsupported reuse cannot invent an owner"
        );
    }
    let refused = rows(
        &database,
        "SELECT version_size,status,last_error FROM assets WHERE library='PrimarySync' AND id='asset-owned-master' ORDER BY 1",
    );
    assert_eq!(
        refused.len(),
        2,
        "both numbered renditions remain the refusal control: {refused:?}"
    );
    for row in &refused {
        assert_eq!(
            row[1],
            rusqlite::types::Value::Text("pending".to_string()),
            "unsupported reuse stays pending: {refused:?}"
        );
        let rusqlite::types::Value::Text(error) = &row[2] else {
            panic!("refusal must retain an explanation: {refused:?}");
        };
        assert!(
            error.contains("existing_match_v1")
                && (error.contains("unsupported_provider_proof_without_valid_receipt")
                    || error.contains("no_valid_receipt_or_supported_provider_proof")),
            "refusal names the missing supported proof: {error}"
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

// Recorded-journal recovery must preserve unrelated historical ownership debt.
#[tokio::test]
async fn mixed_recorded_recovery_preserves_legacy_debt_without_new_reservations() {
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
            if id == "current-a" {
                SeedSource::Verified
            } else {
                SeedSource::Unknown
            },
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
        unresolved_retry_tail: false,
        requests: Arc::new(AtomicUsize::new(0)),
        hydrations: Arc::new(AtomicUsize::new(0)),
        tail_changes: Arc::new(AtomicUsize::new(0)),
        tail_readiness: Arc::new(AtomicUsize::new(0)),
        tail_data_queries: Arc::new(AtomicUsize::new(0)),
        tail_lookup_sequence: Arc::new(AtomicUsize::new(0)),
        priming: false,
        cycle: 0,
        expected_cursor: None,
        fault_active: false,
        next_expected: Arc::new(Mutex::new(None)),
        next_page: Arc::new(Mutex::new(0)),
        witness: Arc::new(Mutex::new(Vec::new())),
        page_hits: Arc::new(Mutex::new(Vec::new())),
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
    let result = crate::download::recover_recorded_sources(
        &reqwest::Client::new(),
        &passes,
        config,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        result.complete,
        "recorded journal recovery finishes without layout planning"
    );
    assert_eq!(
        result.stats.downloaded, 0,
        "recovery does not copy existing media"
    );
    assert_eq!(
        rows(
            &database,
            "SELECT * FROM metadata_capture_retries WHERE asset_id='legacy-master'"
        )
        .len(),
        1,
        "journal recovery cannot clear historical capture debt"
    );
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
