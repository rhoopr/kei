//! Actual SIGKILL between durable production transitions, followed by reopen,
//! production recovery, and two quiet cycles. This proves process death only;
//! it does not model storage caches or power loss.
use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::db::account::AccountOwner;
use crate::state::db::provider_selection::SelectionPath;
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::{download, state};

const POINTS: [&str; 6] = [
    "journal-displaced",
    "journal-installed",
    "journal-committed",
    "published",
    "state-persisted",
    "checkpoint-persisted",
];
const WORKER: &str = "sync_loop::runner::tests::process_death::process_death_worker";
const ZONE: &str = "PrimarySync";
const FOREIGN: &str = "SharedSync-DEATH";
const ID: &str = "asset-death";
const MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
const PEER: &[u8] =
    b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xfe\0\x04peer\xff\xd9";
const OLD: &[u8] = b"\xff\xd8\xff";
const PRIVATE: &[u8] = b"unselected zone: private bytes";

fn checksum(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}
fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "process-death@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-process-death"}})).unwrap(),
    )
    .unwrap()
}
fn zone() -> Value {
    json!({"zoneName": ZONE, "ownerRecordName": "_defaultOwner"})
}
fn hash(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}
fn target(root: &Path) -> PathBuf {
    let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
    root.join("media")
        .join(
            date.with_timezone(&chrono::Local)
                .format("%Y/%m/%d")
                .to_string(),
        )
        .join("photo.JPG")
}

fn peer_target(root: &Path) -> PathBuf {
    let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
    root.join("media")
        .join(
            date.with_timezone(&chrono::Local)
                .format("%Y/%m/%d")
                .to_string(),
        )
        .join("peer.JPG")
}
fn rows(root: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
fn count(root: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
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
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}
fn file_stamps(
    root: &Path,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    files
        .keys()
        .map(|path| {
            let metadata = std::fs::metadata(root.join(path)).unwrap();
            (path.clone(), (metadata.ino(), metadata.modified().unwrap()))
        })
        .collect()
}

fn durable(root: &Path) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    ["SELECT library,id,version_size,status,local_path,local_checksum,last_error,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum FROM assets ORDER BY 1,2,3",
     "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum FROM asset_metadata_paths ORDER BY 1,2,3,4",
     "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings ORDER BY 1,2",
     "SELECT library,asset_id FROM metadata_capture_retries ORDER BY 1,2",
     "SELECT path FROM owned_temp_files ORDER BY path",
     "SELECT * FROM download_receipts ORDER BY account_key,provider_key,scope,library,child,state_id,version_size,native_path",
     "SELECT id,status,interrupted FROM sync_runs WHERE status!='complete' ORDER BY id",
     "SELECT key,value FROM metadata WHERE key LIKE 'sync_token:%' OR key LIKE 'pending_sync_token:%' OR key LIKE 'unresolved_asset_identity:%' ORDER BY key"]
        .iter().map(|sql| rows(root, sql)).collect()
}

fn assert_owned_receipts(root: &Path) {
    use rusqlite::types::Value as Sql;
    let expected_receipt = vec![vec![
        Sql::Text(target(root).to_str().unwrap().to_string()),
        Sql::Text(hash(MEDIA)),
        Sql::Text(checksum(MEDIA)),
    ]];
    assert_eq!(
        rows(
            root,
            "SELECT local_path,local_checksum,checksum FROM assets WHERE library='PrimarySync'"
        ),
        expected_receipt,
        "exact catalogue ownership and checksums"
    );
    assert_eq!(
        rows(
            root,
            "SELECT local_path,local_checksum,provider_checksum FROM asset_metadata_paths WHERE library='PrimarySync'"
        ),
        expected_receipt,
        "exact publication ownership and checksums"
    );
    assert_eq!(
        count(
            root,
            "SELECT count(*) FROM assets WHERE library='PrimarySync' AND (metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL)"
        ),
        0,
        "repair markers drained"
    );
}

fn prepared_publications(root: &Path) -> Vec<(String, Value)> {
    use rusqlite::types::Value as Sql;

    rows(
        root,
        "SELECT path,publication FROM owned_temp_files WHERE publication IS NOT NULL",
    )
    .into_iter()
    .map(|row| {
        let Sql::Blob(path) = &row[0] else {
            panic!("owned temporary path type");
        };
        let Sql::Text(publication) = &row[1] else {
            panic!("prepared publication type");
        };
        (
            String::from_utf8(path.clone()).expect("synthetic UTF-8 temporary path"),
            serde_json::from_str(publication).expect("prepared publication JSON"),
        )
    })
    .collect()
}

fn assert_prepared_publication(root: &Path, point: &str) {
    use rusqlite::types::Value as Sql;

    let publications = prepared_publications(root);
    assert_eq!(
        publications.len(),
        1,
        "exact prepared publication cardinality at {point}"
    );
    let (owned_path, publication) = &publications[0];
    let target = target(root);
    let expected_scope = serde_json::to_string(&json!({
        "format": 1,
        "realm": "com",
        "container": "com.apple.photos.cloud",
        "environment": "production",
        "database": "private",
        "zone": zone(),
    }))
    .unwrap();
    let fixture_owner = owner();
    let (account_key, provider_key) = fixture_owner.authenticated_keys().unwrap();
    let binding = &publication["binding"];
    assert_eq!(binding["account_key"], json!(account_key), "{point}");
    assert_eq!(binding["provider_key"], json!(provider_key), "{point}");
    assert_eq!(binding["scope"], json!(expected_scope), "{point}");
    assert_eq!(binding["library"], json!(ZONE), "{point}");
    assert_eq!(binding["child"], json!(ID), "{point}");
    assert_eq!(binding["master"], json!("death"), "{point}");
    assert_eq!(binding["state_id"], json!(ID), "{point}");
    let pass = binding["origin_pass"]
        .as_str()
        .expect("prepared binding pass");
    assert_eq!(pass.len(), 64, "prepared binding pass length at {point}");
    assert!(
        pass.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "prepared binding pass identity at {point}"
    );
    let decisions = rows(
        root,
        "SELECT pass_key FROM provider_active_decisions WHERE child='asset-death'",
    );
    if let Some(decision) = decisions.first() {
        assert_eq!(decision[0], Sql::Text(pass.to_owned()), "{point}");
    }
    assert_eq!(publication["library"], json!(ZONE), "{point}");
    assert_eq!(publication["id"], json!(ID), "{point}");
    assert_eq!(publication["version"], json!("original"), "{point}");
    assert_eq!(
        publication["provider_checksum"],
        json!(checksum(MEDIA)),
        "{point}"
    );
    assert_eq!(publication["provider_size"], json!(MEDIA.len()), "{point}");
    assert_eq!(publication["local_checksum"], json!(hash(MEDIA)), "{point}");
    assert_eq!(
        publication["source_checksum"],
        json!(hash(MEDIA)),
        "{point}"
    );
    assert_eq!(
        publication["destination"],
        serde_json::to_value(SelectionPath::from_path(&target)).unwrap(),
        "{point}"
    );
    let temporary = publication["temporary"]["path"]
        .as_str()
        .expect("prepared temporary path");
    assert_eq!(owned_path, temporary, "{point}");
    assert_eq!(
        Path::new(temporary).parent(),
        target.parent(),
        "prepared temporary parent at {point}"
    );
    let name = Path::new(temporary)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("prepared temporary filename");
    assert!(name.starts_with("kei-v1-"), "{point}");
    assert!(name.ends_with(".kei-tmp"), "{point}");
    eprintln!(
        "PREPARED-PROOF point={point} id={} version={} provider_size={} destination={} temporary={} pass={}",
        publication["id"],
        publication["version"],
        publication["provider_size"],
        target.display(),
        temporary,
        pass
    );
}

fn assert_compact_receipt(root: &Path, pass: &str) {
    use rusqlite::types::Value as Sql;

    let fixture_owner = owner();
    let (account_key, provider_key) = fixture_owner.authenticated_keys().unwrap();
    let expected_scope = serde_json::to_string(&json!({
        "format": 1,
        "realm": "com",
        "container": "com.apple.photos.cloud",
        "environment": "production",
        "database": "private",
        "zone": zone(),
    }))
    .unwrap();
    assert_eq!(
        rows(
            root,
            "SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,generation_id FROM download_receipts WHERE state_id='asset-death'",
        ),
        vec![vec![
            Sql::Text(account_key.to_owned()),
            Sql::Text(provider_key.to_owned()),
            Sql::Text(expected_scope),
            Sql::Text(ZONE.to_owned()),
            Sql::Text(ID.to_owned()),
            Sql::Text("death".to_owned()),
            Sql::Text(ID.to_owned()),
            Sql::Text(pass.to_owned()),
            Sql::Text("original".to_owned()),
            Sql::Text(checksum(MEDIA)),
            Sql::Integer(MEDIA.len() as i64),
            Sql::Text(SelectionPath::from_path(&target(root)).key().unwrap()),
            Sql::Text(hash(MEDIA)),
            Sql::Text(hash(MEDIA)),
            Sql::Null,
        ]],
        "exact compact publication receipt and provenance"
    );
}

fn assert_prime_receipt(root: &Path) {
    use rusqlite::types::Value as Sql;
    let target = target(root);
    let expected_scope = serde_json::to_string(&json!({
        "format": 1,
        "realm": "com",
        "container": "com.apple.photos.cloud",
        "environment": "production",
        "database": "private",
        "zone": zone(),
    }))
    .unwrap();
    let fixture_owner = owner();
    let (account_key, provider_key) = fixture_owner.authenticated_keys().unwrap();
    let native = SelectionPath::from_path(&target).key().unwrap();
    assert_eq!(
        count(
            root,
            "SELECT count(*) FROM assets WHERE library='PrimarySync' AND id='asset-death' AND version_size='original' AND status='downloaded'"
        ),
        1,
        "prime asset row"
    );
    assert_eq!(
        count(root, "SELECT count(*) FROM download_receipts"),
        1,
        "prime compact receipt cardinality"
    );
    let decision = rows(
        root,
        "SELECT generation,pass_key,child,outcome,admission,manifest,manifest_hash FROM provider_active_decisions WHERE child='asset-death'",
    );
    assert_eq!(decision.len(), 1, "prime decision cardinality");
    assert_eq!(decision[0][2], Sql::Text(ID.to_owned()));
    assert_eq!(decision[0][3], Sql::Text("selected".to_owned()));
    assert_eq!(decision[0][4], Sql::Text("admitted".to_owned()));
    let Sql::Text(pass_key) = &decision[0][1] else {
        panic!("prime pass key type");
    };
    assert_eq!(pass_key.len(), 64, "prime pass key length");
    let Sql::Blob(manifest) = &decision[0][5] else {
        panic!("prime decision manifest type");
    };
    let manifest: Value = serde_json::from_slice(manifest).unwrap();
    assert_eq!(manifest["state_id"], json!(ID), "prime decision root");
    assert_eq!(manifest["sources"], json!([]), "prime decision sources");
    assert_eq!(manifest["decision"]["child"], json!(ID));
    assert_eq!(manifest["decision"]["pass_key"], json!(pass_key));
    assert_eq!(
        manifest["decision"]["destinations"][0]["path"],
        serde_json::to_value(SelectionPath::from_path(&target)).unwrap(),
        "prime manifest destination"
    );
    assert_eq!(
        manifest["decision"]["destinations"][0]["checksum"],
        json!(checksum(MEDIA))
    );
    assert_eq!(
        manifest["decision"]["destinations"][0]["size"],
        json!(MEDIA.len())
    );
    let Sql::Text(manifest_hash) = &decision[0][6] else {
        panic!("prime manifest hash type");
    };
    assert_eq!(manifest_hash.len(), 64, "prime manifest hash length");
    let destination = rows(
        root,
        "SELECT generation,pass_key,child,library,asset_id,master,version_size,path,compat_path,checksum,size_bytes,admitted,verified_media,local_checksum,source_checksum FROM provider_active_destinations WHERE child='asset-death'",
    );
    assert_eq!(destination.len(), 1, "prime destination cardinality");
    assert_eq!(destination[0][1], Sql::Text(pass_key.clone()));
    assert_eq!(destination[0][2], Sql::Text(ID.to_owned()));
    assert_eq!(destination[0][3], Sql::Text(ZONE.to_owned()));
    assert_eq!(destination[0][4], Sql::Text(ID.to_owned()));
    assert_eq!(destination[0][5], Sql::Text("death".to_owned()));
    assert_eq!(destination[0][6], Sql::Text("original".to_owned()));
    assert_eq!(destination[0][7], Sql::Text(native.clone()));
    assert_eq!(destination[0][8], Sql::Text(target.display().to_string()));
    assert_eq!(destination[0][9], Sql::Text(checksum(MEDIA)));
    assert_eq!(destination[0][10], Sql::Integer(MEDIA.len() as i64));
    assert_eq!(destination[0][11], Sql::Integer(1));
    assert_eq!(destination[0][12], Sql::Integer(1));
    assert_eq!(destination[0][13], Sql::Text(hash(MEDIA)));
    assert_eq!(destination[0][14], Sql::Text(hash(MEDIA)));
    let generation = destination[0][0].clone();
    let generations = rows(
        root,
        "SELECT id,account_key,provider_key,scope,specification FROM provider_active_generations",
    );
    let generation_row = generations
        .iter()
        .find(|row| row[0] == generation)
        .expect("prime generation binding");
    assert_eq!(generation_row[1], Sql::Text(account_key.to_owned()));
    assert_eq!(generation_row[2], Sql::Text(provider_key.to_owned()));
    assert_eq!(generation_row[3], Sql::Text(expected_scope.clone()));
    let Sql::Blob(specification) = &generation_row[4] else {
        panic!("prime generation specification type");
    };
    let specification: Value = serde_json::from_slice(specification).unwrap();
    assert_eq!(specification["zone"], zone(), "prime generation zone");
    let sources = rows(
        root,
        "SELECT generation,pass_key,child,page_id,ordinal FROM provider_active_sources WHERE child='asset-death'",
    );
    assert!(
        sources.is_empty(),
        "synthetic inventory has no rank-page sources; the decision manifest is the root"
    );
    assert_compact_receipt(root, pass_key);
    assert_eq!(
        rows(
            root,
            "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-death'"
        ),
        vec![vec![
            Sql::Text(ZONE.to_owned()),
            Sql::Text(ID.to_owned()),
            Sql::Text("original".to_owned()),
            Sql::Text(target.display().to_string()),
            Sql::Text(checksum(MEDIA)),
            Sql::Text(hash(MEDIA)),
            Sql::Text(hash(MEDIA)),
        ]],
        "prime metadata receipt"
    );
    assert_eq!(
        count(root, "SELECT count(*) FROM owned_temp_files"),
        0,
        "prime protected temp retired"
    );
}

#[derive(Clone, Debug)]
struct Provider {
    records: Vec<Value>,
    hydrations: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    quiet: bool,
    successor: String,
    change_token: Option<String>,
    change_records: Vec<Value>,
}
#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        assert!(
            self.requests.fetch_add(1, Ordering::SeqCst) < 80,
            "bounded provider requests"
        );
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            assert_eq!(request["zoneID"], zone());
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.ends_with("/zones/list") {
            assert_eq!(request, json!({}));
            return Ok(
                json!({"zones":[{"zoneID":zone(),"syncToken":"fixture-zone-history-boundary","deleted":false}]}),
            );
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone());
            let token = request["zones"][0]["syncToken"].as_str();
            let records = if token.is_none() {
                self.records.clone()
            } else if token == self.change_token.as_deref() {
                self.change_records.clone()
            } else {
                Vec::new()
            };
            return Ok(
                json!({"zones":[{"zoneID":zone(),"records":records,"syncToken":self.successor,"moreComing":false}]}),
            );
        }
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response(1));
        }
        if url.contains("/records/query?") {
            assert_eq!(request["zoneID"], zone());
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset == 0 {self.records.clone()} else {Vec::new()},"syncToken":self.successor}),
            );
        }
        assert!(url.contains("/records/lookup?"), "unexpected route {url}");
        assert!(!self.quiet, "quiet cycle hydrated");
        self.hydrations.fetch_add(1, Ordering::SeqCst);
        let requested = request["records"].as_array().unwrap();
        Ok(
            json!({"records":self.records.iter().filter(|r| requested.iter().any(|q|q["recordName"]==r["recordName"])).cloned().collect::<Vec<_>>()}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
async fn cycle(root: &Path, url: &str, quiet: bool, successor: &'static str) -> (usize, usize) {
    let mut page = full_album_page_with_download(
        ZONE,
        "death",
        "after",
        url,
        MEDIA.len() as u64,
        &checksum(MEDIA),
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
    cycle_page(root, url, quiet, successor, page, None, Vec::new()).await
}

async fn peer_cycle(
    root: &Path,
    url: &str,
    peer_url: &str,
    quiet: bool,
    successor: &'static str,
) -> (usize, usize) {
    let mut page = full_album_page_with_download(
        ZONE,
        "peer",
        "after-peer",
        peer_url,
        PEER.len() as u64,
        &checksum(PEER),
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] = json!("peer.JPG");
    let records = page["records"].as_array().unwrap().clone();
    cycle_page(
        root,
        url,
        quiet,
        successor,
        page,
        Some("after".to_owned()),
        records,
    )
    .await
}

async fn cycle_page(
    root: &Path,
    url: &str,
    quiet: bool,
    successor: &'static str,
    mut page: Value,
    change_token: Option<String>,
    change_records: Vec<Value>,
) -> (usize, usize) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"] = zone();
    let hydrations = Arc::new(AtomicUsize::new(0));
    let provider = Provider {
        records: page["records"].as_array().unwrap().clone(),
        hydrations: hydrations.clone(),
        requests: Arc::new(AtomicUsize::new(0)),
        quiet,
        successor: successor.to_owned(),
        change_token,
        change_records,
    };
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut params = HashMap::new();
    params.insert("remapEnums".to_string(), json!(true));
    params.insert("getCurrentSyncToken".to_string(), json!(true));
    let service_root = url.strip_suffix("/media").expect("media URL");
    let service_endpoint =
        format!("{service_root}/database/1/com.apple.photos.cloud/production/private");
    let mut library = PhotoLibrary::new(
        service_endpoint,
        Arc::new(params),
        Box::new(provider.clone()),
        Arc::new(zone()),
        "private".to_owned(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(Arc::clone(&db), owner(), "com"));
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::default(),
    };
    let mut library_state =
        make_run_cycle_library_state_with_passes(ZONE, "sync_token:PrimarySync", vec![pass]);
    library_state.library = library;
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let media = root.join("media");
    let base = make_run_cycle_download_config_builder_with_options(
        &media,
        store.clone(),
        RunCycleDownloadConfigOptions {
            concurrent_downloads: Some(1),
            #[cfg(feature = "xmp")]
            xmp_sidecar: true,
            ..Default::default()
        },
    );
    let build = |mode, excluded, groups, zone| {
        let mut built = base(mode, excluded, groups, zone);
        Arc::make_mut(&mut built).repair_truncated = true;
        built
    };
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut config = make_run_cycle_config();
    config.runtime.repair_truncated = true;
    let result = Box::pin(crate::sync_cycle::run_cycle(
        &[&library_state],
        &config,
        Some(store.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &session,
        &CancellationToken::new(),
    ))
    .await
    .unwrap();
    assert_eq!(
        result.failed_count,
        0,
        "{result:?} events={:?}",
        trace.events()
    );
    assert!(!result.session_expired);
    assert!(result.db_sync_token_advance_safe);
    assert_eq!(result.stats.exif_failures, 0);
    if quiet {
        assert_eq!(
            result.stats.metadata_capture_refreshed, 0,
            "quiet metadata repair"
        );
        assert!(
            !result.stats.metadata_capture_progressed,
            "quiet capture progression"
        );
    }
    drop(base);
    drop(library_state);
    drop(store);
    drop(db);
    (result.stats.downloaded, hydrations.load(Ordering::SeqCst))
}
async fn seed(root: &Path) {
    std::fs::create_dir_all(root.join("media")).unwrap();
    std::fs::write(root.join("fixture-owner"), b"kei-synthetic-process-death").unwrap();
    let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
        .await
        .unwrap();
    let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
    let path = root.join("media/foreign.jpg");
    std::fs::write(&path, PRIVATE).unwrap();
    std::fs::write(
        root.join("media/foreign.jpg.xmp"),
        b"private unrelated sidecar",
    )
    .unwrap();
    let row = crate::test_helpers::TestAssetRecord::new(ID)
        .library(FOREIGN)
        .filename("foreign.jpg")
        .created_at(date)
        .added_at(date)
        .size(PRIVATE.len() as u64)
        .checksum(&checksum(PRIVATE))
        .build();
    db.upsert_seen(&row).await.unwrap();
    db.upsert_asset_master_mapping(FOREIGN, ID, "death")
        .await
        .unwrap();
    db.mark_downloaded(
        FOREIGN,
        ID,
        "original",
        &path,
        &hash(PRIVATE),
        Some(&hash(PRIVATE)),
    )
    .await
    .unwrap();
    db.set_metadata("sync_token:SharedSync-DEATH", "foreign-before")
        .await
        .unwrap();
    drop(db);
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn process_death_recovery_matrix() {
    let replay = std::env::var("KEI_PROCESS_DEATH_REPLAY").ok();
    if let Some(point) = &replay {
        assert!(
            POINTS.contains(&point.as_str()),
            "unknown fixed replay point"
        );
    }
    for point in POINTS
        .into_iter()
        .filter(|p| replay.as_deref().is_none_or(|r| r == *p))
    {
        eprintln!(
            "KEI_PROCESS_DEATH_REPLAY={point} cargo test --lib process_death_recovery_matrix -- --nocapture"
        );
        // A failed bind fails this proof honestly.
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/media"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/peer-media"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PEER))
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let repair = point.starts_with("journal-");
        seed(root.path()).await;
        let mut prime_requests = Vec::new();
        if repair {
            // Establish all config, identity, metadata and publication receipts
            // through production before simulating an explicitly approved repair.
            assert_eq!(
                cycle(
                    root.path(),
                    &format!("{}/media", server.uri()),
                    false,
                    "after-prime"
                )
                .await
                .0,
                1
            );
            prime_requests = server.received_requests().await.unwrap();
            assert_eq!(prime_requests.len(), 1, "prime transfer identity");
            assert!(
                prime_requests[0].url.path().ends_with("/media"),
                "prime transfer identity"
            );
            assert_prime_receipt(root.path());
            assert_eq!(std::fs::read(target(root.path())).unwrap(), MEDIA);
            std::fs::write(target(root.path()), OLD).unwrap();
            let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner())
                .await
                .unwrap();
            db.mark_failed(
                ZONE,
                ID,
                "original",
                crate::commands::reconcile::FILE_TRUNCATED_REASON,
            )
            .await
            .unwrap();
            drop(db);
        }
        let foreign = rows(
            root.path(),
            "SELECT * FROM assets WHERE library='SharedSync-DEATH'",
        );
        let foreign_receipt = rows(
            root.path(),
            "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-DEATH'",
        );
        let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", WORKER, "--ignored", "--nocapture"])
                .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
                .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                .env(
                    "KEI_TEST_PROCESS_DEATH_URL",
                    format!("{}/media", server.uri()),
                )
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited before {point}: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "worker timeout at {point}: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("ready")).unwrap(),
            point
        );
        worker.0.kill().unwrap();
        assert_eq!(
            worker.0.wait().unwrap().signal(),
            Some(9),
            "actual SIGKILL without unwinding"
        );
        let checkpoint = count(
            root.path(),
            "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'",
        );
        assert_eq!(
            checkpoint,
            i64::from(point == "checkpoint-persisted"),
            "checkpoint cannot precede durable completion"
        );
        let persisted = count(
            root.path(),
            "SELECT count(*) FROM assets WHERE library='PrimarySync' AND status='downloaded'",
        );
        assert_eq!(
            persisted,
            i64::from(matches!(point, "state-persisted" | "checkpoint-persisted")),
            "finalization boundary"
        );
        if persisted == 1 {
            assert_owned_receipts(root.path());
        }
        let journal_files = files(&root.path().join("media"));
        let journals = journal_files
            .keys()
            .filter(|p| {
                p.components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with(".kei-replace-"))
            })
            .count();
        assert_eq!(journals > 0, repair, "real fallback journal survives death");
        if point == "journal-displaced" {
            assert!(!target(root.path()).exists());
        } else {
            assert_eq!(
                std::fs::read(target(root.path())).unwrap(),
                MEDIA,
                "published media bytes"
            );
        }
        if repair || point == "published" {
            assert_prepared_publication(root.path(), point);
        } else {
            assert!(
                prepared_publications(root.path()).is_empty(),
                "proof retired at {point}"
            );
        }
        assert_eq!(
            count(root.path(), "SELECT count(*) FROM owned_temp_files"),
            i64::from(repair || point == "published"),
            "exact temporary ownership cardinality at {point}"
        );
        let publication_pass = if repair || point == "published" {
            prepared_publications(root.path())[0].1["binding"]["origin_pass"]
                .as_str()
                .unwrap()
                .to_owned()
        } else {
            let receipt = rows(
                root.path(),
                "SELECT origin_pass FROM download_receipts WHERE state_id='asset-death'",
            );
            assert_eq!(receipt.len(), 1, "pre-recovery compact receipt cardinality");
            let rusqlite::types::Value::Text(pass) = &receipt[0][0] else {
                panic!("pre-recovery compact receipt pass type");
            };
            pass.clone()
        };
        if point == "published" {
            assert_eq!(
                count(
                    root.path(),
                    "SELECT count(*) FROM assets WHERE library='PrimarySync' AND id='asset-death' AND status='pending'"
                ),
                1,
                "published crash keeps the asset pending"
            );
            assert_eq!(
                count(root.path(), "SELECT count(*) FROM download_receipts"),
                0,
                "published crash has no current compact receipt"
            );
            assert_eq!(
                count(
                    root.path(),
                    "SELECT count(*) FROM asset_metadata_paths WHERE library='PrimarySync'"
                ),
                0,
                "published crash has no current path finalization"
            );
        }
        if matches!(point, "state-persisted" | "checkpoint-persisted") {
            assert_prime_receipt(root.path());
        }
        if repair {
            assert!(
                journal_files
                    .iter()
                    .any(|(p, b)| p.file_name().is_some_and(|n| n == "original")
                        && b.as_slice() == OLD),
                "original bytes retained before restart"
            );
            assert_eq!(
                journal_files
                    .keys()
                    .any(|p| p.file_name().is_some_and(|n| n == "committed")),
                point == "journal-committed",
                "commit boundary"
            );
        }
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM asset_metadata_paths WHERE library='PrimarySync'"
            ),
            i64::from(repair || persisted == 1),
            "publication receipt boundary"
        );
        // The repair runs in the retry pass after enumeration has completed
        // its tracked run. Initial-download finalization remains inside it.
        let interrupted = matches!(point, "published" | "state-persisted");
        let orphaned = u64::from(interrupted);
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM sync_runs WHERE status='running'"
            ),
            i64::from(interrupted),
            "durable interrupted run"
        );
        {
            // Reuse the production runner's one-time startup recovery owner.
            let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner())
                .await
                .unwrap();
            assert_eq!(db.promote_orphaned_sync_runs().await.unwrap(), orphaned);
            drop(db);
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len() - prime_requests.len(),
            1,
            "worker transfer identity"
        );
        assert!(
            requests.last().unwrap().url.path().ends_with("/media"),
            "worker transfer identity"
        );
        let downloads_before = requests.len();
        eprintln!(
            "FIRST-GATE point={point} owned={:?} assets={:?} receipts={:?} journals={:?}",
            rows(
                root.path(),
                "SELECT path,publication FROM owned_temp_files ORDER BY path"
            ),
            rows(
                root.path(),
                "SELECT id,status,local_path,last_error FROM assets WHERE library='PrimarySync' ORDER BY id"
            ),
            rows(
                root.path(),
                "SELECT state_id,native_path FROM download_receipts WHERE library='PrimarySync' ORDER BY state_id"
            ),
            files(&root.path().join("media"))
                .keys()
                .filter(|p| p
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with(".kei-replace-")))
                .collect::<Vec<_>>()
        );
        let (downloaded, _) = cycle(
            root.path(),
            &format!("{}/media", server.uri()),
            false,
            "after",
        )
        .await;
        assert_eq!(
            downloaded, 0,
            "prepared publication recovery must reuse the verified target or part"
        );
        let recovery_requests = server.received_requests().await.unwrap().len();
        assert_eq!(
            recovery_requests, downloads_before,
            "prepared publication recovery must not issue another media GET"
        );
        eprintln!(
            "PREPARED-RECOVERY point={point} downloaded={downloaded} requests_delta={}",
            recovery_requests.saturating_sub(downloads_before)
        );
        assert_eq!(std::fs::read(target(root.path())).unwrap(), MEDIA);
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM assets WHERE library='PrimarySync' AND status='downloaded' AND last_error IS NULL"
            ),
            1
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM asset_metadata_paths WHERE library='PrimarySync'"
            ),
            1,
            "one owned publication receipt"
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after'"
            ),
            1,
            "first eligible cycle advances"
        );
        assert_eq!(
            count(root.path(), "SELECT count(*) FROM metadata_capture_retries"),
            0
        );
        let claims = count(root.path(), "SELECT count(*) FROM owned_temp_files");
        assert_eq!(
            claims, 0,
            "first recovery retires the prepared proof before cleanup grace"
        );
        assert_owned_receipts(root.path());
        assert_compact_receipt(root.path(), &publication_pass);
        let stable_files = files(&root.path().join("media"));
        for path in stable_files.keys().filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "kei-tmp")
        }) {
            let metadata = std::fs::metadata(root.path().join("media").join(path)).unwrap();
            let target_metadata = std::fs::metadata(target(root.path())).unwrap();
            eprintln!(
                "PREPARED-ORPHAN point={point} path={} size={} links={} same_inode={}",
                path.display(),
                metadata.len(),
                metadata.nlink(),
                metadata.ino() == target_metadata.ino(),
            );
        }
        assert_eq!(
            stable_files.len(),
            3 + usize::from(cfg!(feature = "xmp")),
            "exact media and sidecar files; no duplicate or orphan publication; files={:?}",
            stable_files.keys().collect::<Vec<_>>()
        );
        assert!(
            !stable_files.keys().any(|p| p
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with(".kei-replace-"))),
            "journal drained"
        );
        #[cfg(feature = "xmp")]
        {
            use xmp_toolkit::{XmpMeta, xmp_ns};
            let sidecar =
                std::fs::read_to_string(target(root.path()).with_file_name("photo.JPG.xmp"))
                    .unwrap();
            let metadata = sidecar.parse::<XmpMeta>().expect("valid recovered sidecar");
            let expected = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS)
                .unwrap()
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            assert_eq!(
                metadata
                    .property(xmp_ns::EXIF, "DateTimeOriginal")
                    .unwrap()
                    .value,
                expected
            );
        }
        assert_eq!(
            std::fs::read(root.path().join("media/foreign.jpg.xmp")).unwrap(),
            b"private unrelated sidecar"
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM sync_runs WHERE status='running'"
            ),
            0
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM sync_runs WHERE status='interrupted' AND interrupted=1"
            ),
            i64::from(interrupted)
        );
        let stable_stamps = file_stamps(&root.path().join("media"), &stable_files);
        let stable_state = durable(root.path());
        let stable_requests = server.received_requests().await.unwrap().len();
        assert_eq!(
            stable_requests, downloads_before,
            "prepared recovery and quiet cycles must not download"
        );
        for _ in 0..2 {
            assert_eq!(
                cycle(
                    root.path(),
                    &format!("{}/media", server.uri()),
                    true,
                    "after"
                )
                .await,
                (0, 0)
            );
            assert_eq!(durable(root.path()), stable_state, "quiet durable state");
            assert_eq!(
                file_stamps(&root.path().join("media"), &stable_files),
                stable_stamps,
                "quiet file identity and modification time; no identical metadata rewrite"
            );
            assert_eq!(
                files(&root.path().join("media")),
                stable_files,
                "quiet bytes and file count"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                stable_requests,
                "quiet tail cannot redownload"
            );
        }
        assert_eq!(
            rows(
                root.path(),
                "SELECT * FROM assets WHERE library='SharedSync-DEATH'"
            ),
            foreign
        );
        assert_eq!(
            rows(
                root.path(),
                "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-DEATH'"
            ),
            foreign_receipt
        );
        assert_eq!(
            std::fs::read(root.path().join("media/foreign.jpg")).unwrap(),
            PRIVATE
        );
        assert_eq!(
            count(
                root.path(),
                "SELECT count(*) FROM metadata WHERE key='sync_token:SharedSync-DEATH' AND value='foreign-before'"
            ),
            1
        );
        if point == "journal-committed" {
            let recovered_asset = rows(
                root.path(),
                "SELECT * FROM assets WHERE library='PrimarySync' AND id='asset-death'",
            );
            let recovered_metadata = rows(
                root.path(),
                "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-death'",
            );
            let recovered_receipt = rows(
                root.path(),
                "SELECT * FROM download_receipts WHERE library='PrimarySync' AND state_id='asset-death'",
            );
            let peer_url = format!("{}/peer-media", server.uri());
            let (peer_downloaded, peer_hydrations) = peer_cycle(
                root.path(),
                &format!("{}/media", server.uri()),
                &peer_url,
                false,
                "after-peer",
            )
            .await;
            assert_eq!(peer_downloaded, 1, "healthy provider peer downloads once");
            assert!(
                peer_hydrations >= 1,
                "healthy provider peer hydrates from the new provider successor"
            );
            let peer_rows = rows(
                root.path(),
                "SELECT library,id,version_size,status,local_path,checksum,local_checksum,download_checksum,last_error FROM assets WHERE library='PrimarySync' AND id='asset-peer'",
            );
            let peer_path = peer_target(root.path());
            assert_eq!(std::fs::read(&peer_path).unwrap(), PEER);
            let peer_native = SelectionPath::from_path(&peer_path).key().unwrap();
            assert_eq!(
                peer_rows,
                vec![vec![
                    rusqlite::types::Value::Text(ZONE.to_owned()),
                    rusqlite::types::Value::Text("asset-peer".to_owned()),
                    rusqlite::types::Value::Text("original".to_owned()),
                    rusqlite::types::Value::Text("downloaded".to_owned()),
                    rusqlite::types::Value::Text(peer_path.display().to_string()),
                    rusqlite::types::Value::Text(checksum(PEER)),
                    rusqlite::types::Value::Text(hash(PEER)),
                    rusqlite::types::Value::Text(hash(PEER)),
                    rusqlite::types::Value::Null,
                ]],
                "healthy provider peer asset"
            );
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-peer'"
                ),
                vec![vec![
                    rusqlite::types::Value::Text(ZONE.to_owned()),
                    rusqlite::types::Value::Text("asset-peer".to_owned()),
                    rusqlite::types::Value::Text("original".to_owned()),
                    rusqlite::types::Value::Text(peer_path.display().to_string()),
                    rusqlite::types::Value::Text(checksum(PEER)),
                    rusqlite::types::Value::Text(hash(PEER)),
                    rusqlite::types::Value::Text(hash(PEER)),
                ]],
                "healthy provider peer metadata"
            );
            let peer_receipt = rows(
                root.path(),
                "SELECT state_id,native_path,provider_checksum,provider_size,local_checksum,source_checksum,origin_pass FROM download_receipts WHERE library='PrimarySync' AND state_id='asset-peer'",
            );
            assert_eq!(peer_receipt.len(), 1, "healthy peer receipt cardinality");
            assert_eq!(
                peer_receipt[0][0],
                rusqlite::types::Value::Text("asset-peer".to_owned())
            );
            assert_eq!(
                peer_receipt[0][1],
                rusqlite::types::Value::Text(peer_native)
            );
            assert_eq!(
                peer_receipt[0][2],
                rusqlite::types::Value::Text(checksum(PEER))
            );
            assert_eq!(
                peer_receipt[0][3],
                rusqlite::types::Value::Integer(PEER.len() as i64)
            );
            assert_eq!(peer_receipt[0][4], rusqlite::types::Value::Text(hash(PEER)));
            assert_eq!(peer_receipt[0][5], rusqlite::types::Value::Text(hash(PEER)));
            let peer_pass = match &peer_receipt[0][6] {
                rusqlite::types::Value::Text(pass) => pass,
                other => panic!("peer receipt pass type {other:?}"),
            };
            assert_eq!(peer_pass.len(), 64, "peer receipt pass identity");
            let fixture_owner = owner();
            let (account_key, provider_key) = fixture_owner.authenticated_keys().unwrap();
            let expected_scope = serde_json::to_string(&json!({
                "format": 1,
                "realm": "com",
                "container": "com.apple.photos.cloud",
                "environment": "production",
                "database": "private",
                "zone": zone(),
            }))
            .unwrap();
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT account_key,provider_key,scope,child,master,version_size FROM download_receipts WHERE state_id='asset-peer'"
                ),
                vec![vec![
                    rusqlite::types::Value::Text(account_key.to_owned()),
                    rusqlite::types::Value::Text(provider_key.to_owned()),
                    rusqlite::types::Value::Text(expected_scope),
                    rusqlite::types::Value::Text("asset-peer".to_owned()),
                    rusqlite::types::Value::Text("peer".to_owned()),
                    rusqlite::types::Value::Text("original".to_owned()),
                ]],
                "healthy peer authenticated receipt binding"
            );
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT d.pass_key,d.outcome,d.admission,p.path,p.checksum,p.size_bytes,p.verified_media,p.local_checksum,p.source_checksum FROM provider_active_decisions d JOIN provider_active_destinations p ON p.generation=d.generation AND p.pass_key=d.pass_key AND p.child=d.child WHERE d.child='asset-peer'"
                ),
                vec![vec![
                    rusqlite::types::Value::Text(peer_pass.clone()),
                    rusqlite::types::Value::Text("selected".to_owned()),
                    rusqlite::types::Value::Text("admitted".to_owned()),
                    rusqlite::types::Value::Text(
                        SelectionPath::from_path(&peer_path).key().unwrap()
                    ),
                    rusqlite::types::Value::Text(checksum(PEER)),
                    rusqlite::types::Value::Integer(PEER.len() as i64),
                    rusqlite::types::Value::Integer(1),
                    rusqlite::types::Value::Text(hash(PEER)),
                    rusqlite::types::Value::Text(hash(PEER)),
                ]],
                "healthy peer current selection and verified native destination"
            );
            assert_eq!(
                count(
                    root.path(),
                    "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync' AND value='after-peer'"
                ),
                1,
                "healthy peer advances its own provider successor"
            );
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT * FROM assets WHERE library='PrimarySync' AND id='asset-death'"
                ),
                recovered_asset
            );
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-death'"
                ),
                recovered_metadata
            );
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT * FROM download_receipts WHERE library='PrimarySync' AND state_id='asset-death'"
                ),
                recovered_receipt
            );
            assert_eq!(
                file_stamps(&root.path().join("media"), &stable_files),
                stable_stamps,
                "healthy peer preserves recovered and foreign file identity"
            );
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT * FROM assets WHERE library='SharedSync-DEATH'"
                ),
                foreign
            );
            assert_eq!(
                rows(
                    root.path(),
                    "SELECT * FROM asset_metadata_paths WHERE library='SharedSync-DEATH'"
                ),
                foreign_receipt
            );
            assert_eq!(
                std::fs::read(root.path().join("media/foreign.jpg")).unwrap(),
                PRIVATE
            );
            let peer_requests = server.received_requests().await.unwrap();
            assert_eq!(
                peer_requests.len(),
                stable_requests + 1,
                "healthy peer uses exactly one new media GET"
            );
            assert_eq!(
                peer_requests.last().unwrap().url.path(),
                "/peer-media",
                "healthy peer fetch identity"
            );
            let post_peer_files = files(&root.path().join("media"));
            assert_eq!(
                post_peer_files.len(),
                stable_files.len() + 1 + usize::from(cfg!(feature = "xmp")),
                "healthy peer adds only its media and optional sidecar"
            );
            let post_peer_stamps = file_stamps(&root.path().join("media"), &post_peer_files);
            let post_peer_state = durable(root.path());
            let post_peer_requests = peer_requests.len();
            for _ in 0..2 {
                assert_eq!(
                    cycle(
                        root.path(),
                        &format!("{}/media", server.uri()),
                        true,
                        "after-peer"
                    )
                    .await,
                    (0, 0)
                );
                assert_eq!(
                    durable(root.path()),
                    post_peer_state,
                    "healthy peer quiet durable state"
                );
                assert_eq!(
                    file_stamps(&root.path().join("media"), &post_peer_files),
                    post_peer_stamps,
                    "healthy peer quiet file identity"
                );
                assert_eq!(
                    files(&root.path().join("media")),
                    post_peer_files,
                    "healthy peer quiet bytes"
                );
                assert_eq!(
                    server.received_requests().await.unwrap().len(),
                    post_peer_requests,
                    "healthy peer quiet tail cannot redownload"
                );
            }
            eprintln!(
                "HEALTHY-PEER point={point} downloaded=1 successor=after-peer receipt=bound foreign=stable quiet=2"
            );
        }
    }
}

#[tokio::test]
#[ignore = "dedicated synthetic subprocess; parent kills it at a fixed durable point"]
async fn process_death_worker() {
    let root =
        PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").expect("parent fixture"));
    let url = std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap();
    cycle(&root, &url, false, "after").await;
    panic!("worker completed without reaching requested death point");
}
