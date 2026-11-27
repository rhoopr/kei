//! Actual CLI account-bound state paths with overlapping synthetic identities.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::support::{clean_cmd, create_state_db, insert_asset, sanitize_username};

const ACCOUNTS: [&str; 2] = ["first.last@example.invalid", "firstlast@example.invalid"];

fn provider(label: &str) -> String {
    let dsid = format!("synthetic-provider-{label}");
    let mut hash = Sha256::new();
    hash.update(b"authenticated-provider-v1");
    for value in ["com", dsid.as_str()] {
        hash.update(value.len().to_string().as_bytes());
        hash.update(b":");
        hash.update(value.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

struct Account {
    username: &'static str,
    label: &'static str,
    path: PathBuf,
    media: PathBuf,
    config: PathBuf,
}

fn seed(root: &Path, username: &'static str, label: &'static str, server: &str) -> Account {
    let media_dir = root.join(format!("media-{label}"));
    std::fs::create_dir(&media_dir).unwrap();
    let media = media_dir.join(format!("{label}.jpg"));
    let bytes = format!("known media belonging only to account {label}").into_bytes();
    std::fs::write(&media, &bytes).unwrap();
    let local_checksum = format!("{:x}", Sha256::digest(&bytes));
    let provider_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));
    let conn = create_state_db(root, username);
    conn.execute(
        "UPDATE account_owner SET provider_key=?1",
        [provider(label)],
    )
    .unwrap();
    insert_asset(
        &conn,
        "overlap",
        "downloaded",
        &format!("{label}.jpg"),
        Some(media.to_str().unwrap()),
        None,
        Some(&local_checksum),
    );
    insert_asset(
        &conn,
        "retry",
        "pending",
        &format!("{label}-retry.jpg"),
        None,
        Some(&format!("{label}-retry-error")),
        None,
    );
    conn.execute(
        "UPDATE assets SET size_bytes=?1,checksum=?2,metadata_hash=?3 WHERE id='overlap'",
        params![
            bytes.len() as i64,
            provider_checksum,
            format!("{label}-completed-metadata")
        ],
    )
    .unwrap();
    // Completed per-path publication receipts. Prepared repair receipts would
    // intentionally queue a metadata rewrite, which is separate sync policy.
    conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum) VALUES ('PrimarySync','overlap','original',?1,?2,?3,?3)",
        params![media.to_str().unwrap(),provider_checksum,local_checksum]).unwrap();
    // The fixture represents already captured current metadata, not a
    // released-schema row needing unrelated provider backfill during sync.
    conn.execute("INSERT INTO asset_metadata_capture_revisions(library,asset_id,revision,updated_at) VALUES ('PrimarySync','overlap',1,1700000000)",[]).unwrap();
    conn.execute("INSERT INTO metadata(key,value) VALUES ('sync_token:PrimarySync',?1),('pending_sync_token:old:PrimarySync',?2)",params![format!("{label}-cursor"),format!("{label}-historical-debt")]).unwrap();
    drop(conn);
    let namespace = sanitize_username(username);
    std::fs::write(
        root.join(format!("{namespace}.session")),
        br#"{"session_token":"synthetic-session"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join(format!("{namespace}.cache")),
        serde_json::to_vec(&json!({
        "validated_at":chrono::Utc::now().timestamp(),"account_data":{
            "dsInfo":{"dsid":format!("synthetic-provider-{label}")},
            "webservices":{"ckdatabasews":{"url":format!("{server}/{label}")}}
        }}))
        .unwrap(),
    )
    .unwrap();
    // Explicit empty password source prevents fallback into a real keyring or
    // password prompt if synthetic cached authentication unexpectedly fails.
    let password = root.join(format!("{label}-empty-password"));
    std::fs::write(&password, "").unwrap();
    let config = root.join(format!("{label}.toml"));
    std::fs::write(&config,format!("config_version=1\n[auth]\nusername={}\npassword_file={}\n[download]\ndirectory={}\n[ui]\nfriendly=false\nprogress_bar=false\n",
        crate::common::toml_string(username),crate::common::toml_string(password.to_str().unwrap()),crate::common::toml_string(media_dir.to_str().unwrap()))).unwrap();
    Account {
        username,
        label,
        path: root.join(format!("{namespace}.db")),
        media,
        config,
    }
}

fn rows(path: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let count = statement.column_count();
    statement
        .query_map([], |r| (0..count).map(|i| r.get(i)).collect())
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test]
async fn retired_selection_settings_refuse_before_provider_or_state_writes() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    let original_config = std::fs::read_to_string(&account.config).unwrap();
    let state_bytes = std::fs::read(&account.path).unwrap();
    let media_bytes = std::fs::read(&account.media).unwrap();
    for (section, key, value) in [
        ("filters", "filename_exclude", "[]"),
        ("filters", "filename_exclude", "[\"*.AAE\"]"),
        ("filters", "filename_exclude", "[\"[invalid\"]"),
        ("photos", "edited_naming", "'primary'"),
        ("photos", "edited_naming", "'suffix'"),
        ("download.retry", "per_asset", "0"),
        ("download.retry", "per_asset", "10"),
        ("download.retry", "per_asset", "100"),
        ("download.retry", "per_asset", "'invalid'"),
    ] {
        let config = format!("{original_config}[{section}]\n{key}={value}\n");
        std::fs::write(&account.config, &config).unwrap();
        let output = command(root, &account, &["sync", "--no-progress-bar"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(&format!("Remove {key} from [{section}]")),
            "{error}"
        );
        assert_eq!(std::fs::read(&account.config).unwrap(), config.as_bytes());
        assert_eq!(std::fs::read(&account.path).unwrap(), state_bytes);
        assert_eq!(std::fs::read(&account.media).unwrap(), media_bytes);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn rendition_migration_refuses_before_provider_or_state_writes() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    let original_config = std::fs::read_to_string(&account.config).unwrap();
    let unversioned = original_config
        .lines()
        .filter(|line| !line.starts_with("config_version"))
        .collect::<Vec<_>>()
        .join("\n");
    let state_bytes = std::fs::read(&account.path).unwrap();
    let media_bytes = std::fs::read(&account.media).unwrap();
    let mut configs = vec![
        unversioned.clone(),
        format!("config_version=0\n{unversioned}"),
    ];
    for (section, key, value) in [
        ("photos", "resolution", "'original'"),
        ("photos", "resolution", "'none'"),
        ("photos", "live_resolution", "'medium'"),
        ("photos", "live_photo_mode", "'image-only'"),
        ("photos", "edited", "false"),
        ("photos", "alternative", "false"),
        ("photos", "raw_policy", "'prefer-raw'"),
        ("photos", "force_resolution", "true"),
        ("filters", "media", "['photos']"),
    ] {
        configs.push(format!(
            "config_version=1\n{unversioned}\n[{section}]\n{key}={value}\n"
        ));
    }
    for config in configs {
        std::fs::write(&account.config, &config).unwrap();
        for args in [
            vec!["sync", "--no-progress-bar"],
            vec!["import-existing", "--force-empty"],
            vec!["config", "show"],
        ] {
            let output = command(root, &account, &args);
            assert!(!output.status.success(), "{args:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("config_version = 1"), "{args:?}: {error}");
            assert!(error.contains("full-resolution"), "{args:?}: {error}");
            assert_eq!(std::fs::read(&account.config).unwrap(), config.as_bytes());
            assert_eq!(std::fs::read(&account.path).unwrap(), state_bytes);
            assert_eq!(std::fs::read(&account.media).unwrap(), media_bytes);
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn state(path: &Path) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
    ["SELECT * FROM account_owner",
     "SELECT key,value FROM metadata WHERE key LIKE 'sync_token:%' OR key LIKE 'pending_sync_token:%' ORDER BY key",
     "SELECT library,id,version_size,status,filename,local_path,last_error,checksum,local_checksum,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM assets ORDER BY library,id,version_size",
     "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
     "SELECT * FROM asset_metadata_capture_revisions ORDER BY library,asset_id"].iter().map(|sql|rows(path,sql)).collect()
}

fn command(root: &Path, account: &Account, args: &[&str]) -> std::process::Output {
    clean_cmd()
        .env("KEI_DATA_DIR", root)
        .env("ICLOUD_USERNAME", account.username)
        .env_remove("KEI_PASSWORD_FILE")
        .env_remove("KEI_PASSWORD_COMMAND")
        .env_remove("KEI_FORCE_EMPTY")
        .env_remove("KEI_REQUEST_DUMP_DIR")
        .env_remove("RUST_LOG")
        .args(["--config", account.config.to_str().unwrap()])
        .args(args)
        .timeout(Duration::from_secs(30))
        .output()
        .unwrap()
}

fn success(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[derive(Clone)]
struct OfflinePhotos;
impl Respond for OfflinePhotos {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let path = request.url.path();
        let label = path.split('/').nth(1).unwrap();
        let value = if path.ends_with("/zones/list") {
            json!({"zones":if path.contains("/private/") { vec![json!({"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}})] } else { vec![] }})
        } else if path.ends_with("/changes/database") {
            json!({"syncToken":"offline-database-cursor","moreComing":false,"zones":[]})
        } else if body
            .get("query")
            .and_then(|query| query.get("recordType"))
            .and_then(Value::as_str)
            == Some("CheckIndexingState")
        {
            json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]})
        } else if path.ends_with("/changes/zone") {
            json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":format!("{label}-cursor"),"moreComing":false,"records":[]}]})
        } else if path.ends_with("/records/lookup") {
            json!({"records":body.get("records").unwrap().as_array().unwrap().iter().map(|wanted| json!({"recordName":wanted.get("recordName").unwrap(),"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"serverErrorCode":"UNKNOWN_ITEM"})).collect::<Vec<_>>()})
        } else if path.ends_with("/internal/records/query/batch") {
            json!({"batch":[{"records":[{"fields":{"itemCount":{"value":0}}}]}]})
        } else if path.ends_with("/records/query") {
            json!({"records":[],"syncToken":format!("{label}-cursor")})
        } else {
            panic!("unexpected offline Photos request: {path}");
        };
        ResponseTemplate::new(200).set_body_json(value)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_ownership_overlapping_state_isolated_through_every_cli_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(OfflinePhotos)
        .mount(&server)
        .await;
    let paths: &[&[&str]] = &[
        &["status", "--downloaded", "--pending"],
        &["manifest"],
        &["verify"],
        &["reconcile"],
        &["doctor", "--json"],
        &["import-existing", "--force-empty"],
        &["sync", "--no-progress-bar"],
        &["reset", "sync-token", "--yes"],
        &["reset", "state", "--yes"],
    ];
    for args in paths {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let a = seed(root, ACCOUNTS[0], "A", &server.uri());
        let b = seed(root, ACCOUNTS[1], "B", &server.uri());
        assert_ne!(a.path, b.path);
        let a_state = state(&a.path);
        let b_state = state(&b.path);
        assert_ne!(a_state, b_state);
        let original_b = std::fs::read(&b.path).unwrap();
        // Deliberately place A's complete owner/catalogue/retry/receipt state
        // at B's hashed path. All nine entrypoints must refuse before access.
        std::fs::remove_file(&b.path).unwrap();
        std::fs::copy(&a.path, &b.path).unwrap();
        let before = std::fs::read(&b.path).unwrap();
        let source = std::fs::read(&a.path).unwrap();
        let before_rows = state(&b.path);
        let request_count = server.received_requests().await.unwrap().len();
        let out = command(root, &b, args);
        assert_eq!(
            std::fs::read(&a.media).unwrap(),
            b"known media belonging only to account A",
            "{args:?}: refused original owner media"
        );
        assert_eq!(
            std::fs::read(&b.media).unwrap(),
            b"known media belonging only to account B",
            "{args:?}: refused requesting owner media"
        );
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            diagnostics.contains("account ownership does not match"),
            "{args:?}: {diagnostics}"
        );
        assert!(
            !diagnostics.contains(ACCOUNTS[0]) && !diagnostics.contains(ACCOUNTS[1]),
            "unredacted account diagnostic"
        );
        if args[0] == "doctor" {
            let report: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert!(
                report["checks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| c["name"] == "state_db" && c["status"] == "error")
            );
        } else {
            assert!(!out.status.success(), "{args:?}: owner refusal must fail");
        }
        assert_eq!(
            before,
            std::fs::read(&b.path).unwrap(),
            "{args:?}: refused copy bytes"
        );
        assert_eq!(
            source,
            std::fs::read(&a.path).unwrap(),
            "{args:?}: original owner bytes"
        );
        assert_eq!(
            before_rows,
            state(&b.path),
            "{args:?}: refused rows and receipts"
        );
        let requests = server.received_requests().await.unwrap();
        for request in &requests[request_count..] {
            assert!(
                !request.url.path().ends_with("/changes/zone")
                    && !request.url.path().ends_with("/records/lookup"),
                "{args:?}: must not start a sync cycle"
            );
        }
        if args[0] == "import-existing" {
            assert_eq!(
                requests.len(),
                request_count,
                "import must refuse before provider enumeration"
            );
        }
        // Restore only the correct disposable B snapshot, then prove both
        // legitimate owners can reopen through the same entrypoint.
        std::fs::write(&b.path, original_b).unwrap();
        assert_eq!(state(&b.path), b_state);
        // Genuine owner positive controls through the actual command. A's
        // explicit resets may mutate A, but no route may read or mutate B.
        for (selected, other) in [(&a, &b), (&b, &a)] {
            // A completed sync replaces an account's previous state file with
            // its sync-core file, so compare whichever files the other has.
            let other_files = account_state_files(other);
            let other_state = other.path.exists().then(|| state(&other.path));
            let out = command(root, selected, args);
            success(&out);
            assert_eq!(
                std::fs::read(&a.media).unwrap(),
                b"known media belonging only to account A",
                "{args:?}: A media before fixture restoration"
            );
            assert_eq!(
                std::fs::read(&b.media).unwrap(),
                b"known media belonging only to account B",
                "{args:?}: B media before fixture restoration"
            );
            assert_eq!(
                other.path.exists().then(|| state(&other.path)),
                other_state,
                "{args:?}: other account rows"
            );
            assert_eq!(
                account_state_files(other),
                other_files,
                "{args:?}: other account DB bytes"
            );
            if args[0] == "status" || args[0] == "manifest" {
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(stdout.contains(&format!("{}.jpg", selected.label)));
                assert!(
                    !stdout.contains(&format!("{}.jpg", other.label)),
                    "cross-account output"
                );
            }
            if args[0] == "reset" && args[1] == "state" {
                assert!(!selected.path.exists());
                // Restore only this disposable fixture for the next positive
                // owner and the deliberate misbound phase.
                let restored = seed_restore(root, selected, &server.uri());
                assert_eq!(
                    state(&restored.path),
                    if selected.label == "A" {
                        a_state.clone()
                    } else {
                        b_state.clone()
                    }
                );
            }
        }
        assert_eq!(
            std::fs::read(&a.media).unwrap(),
            b"known media belonging only to account A"
        );
        assert_eq!(
            std::fs::read(&b.media).unwrap(),
            b"known media belonging only to account B"
        );
    }
}

/// The bytes of an account's previous-engine and sync-core state files.
fn account_state_files(account: &Account) -> Vec<(PathBuf, Vec<u8>)> {
    [account.path.clone(), account.path.with_extension("sync.db")]
        .into_iter()
        .filter_map(|path| std::fs::read(&path).ok().map(|bytes| (path, bytes)))
        .collect()
}

fn seed_restore(root: &Path, account: &Account, server: &str) -> Account {
    // Fresh seed reuses existing synthetic media/config directories only.
    std::fs::remove_dir_all(account.media.parent().unwrap()).unwrap();
    seed(root, account.username, account.label, server)
}

#[cfg(any(unix, windows))]
fn migration_backup_bundles(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut saved, mut incomplete): (Vec<_>, Vec<_>) = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".kei-schema-backup-")
        })
        .partition(|path| {
            path.is_dir()
                && path
                    .extension()
                    .is_none_or(|extension| extension != "partial")
                && path.join("manifest.json").is_file()
        });
    saved.sort();
    incomplete.sort();
    (saved, incomplete)
}

#[cfg(any(unix, windows))]
#[test]
fn migration_backup_discovery_requires_final_directory_and_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let complete = root.join(".kei-schema-backup-complete");
    let partial = root.join(".kei-schema-backup-prepared.partial");
    let unmarked = root.join(".kei-schema-backup-unmarked");
    let unrelated = root.join("unrelated");
    for path in [&complete, &partial, &unrelated, &unmarked] {
        std::fs::create_dir(path).unwrap();
    }
    for path in [&complete, &partial, &unrelated] {
        std::fs::write(path.join("manifest.json"), b"{}").unwrap();
    }
    let (saved, mut incomplete) = migration_backup_bundles(root);
    assert_eq!(saved, [complete]);
    incomplete.sort();
    let mut expected = vec![partial.clone(), unmarked];
    expected.sort();
    assert_eq!(incomplete, expected);
    assert_eq!(std::fs::read(partial.join("manifest.json")).unwrap(), b"{}");
}

#[test]
fn account_ownership_exact_alias_spellings_remain_separate_with_same_provider_pin() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let a = seed(root, ACCOUNTS[0], "A", "http://127.0.0.1:1");
    let b = seed(root, ACCOUNTS[1], "B", "http://127.0.0.1:1");
    Connection::open(&b.path)
        .unwrap()
        .execute("UPDATE account_owner SET provider_key=?1", [provider("A")])
        .unwrap();
    let a_state = state(&a.path);
    let b_state = state(&b.path);
    for _ in 0..2 {
        for (selected, other) in [(&a, &b), (&b, &a)] {
            let out = command(root, selected, &["manifest"]);
            success(&out);
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(stdout.contains(&format!("{}.jpg", selected.label)));
            assert!(!stdout.contains(&format!("{}.jpg", other.label)));
        }
        assert_eq!(state(&a.path), a_state);
        assert_eq!(state(&b.path), b_state);
    }
}

// A real service process, real 60-second watch cadence, synthetic account only.
// The receipt fault interrupts between durable capture and stream emission.
const QUIET_MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\x01\0\0\xff\xd9";

#[derive(Clone)]
struct RecentCliPhotos {
    first_delta: std::sync::Arc<std::sync::atomic::AtomicBool>,
    queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RecentCliPhotos {
    fn pair(&self, number: usize) -> Vec<Value> {
        let master = format!("recent-{number}");
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        vec![
            json!({"recordName":master,"recordType":"CPLMaster","zoneID":zone,"fields":{
                "filenameEnc":{"value":format!("recent-{number}.jpg"),"type":"STRING"},
                "resOriginalRes":{"value":{"downloadURL":format!("https://p01.icloud-content.com/synthetic-never-fetch-recent-{number}"),"size":QUIET_MEDIA.len(),
                    "fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(QUIET_MEDIA))}},
                "resOriginalWidth":{"value":1},"resOriginalHeight":{"value":1},"resOriginalFileType":{"value":"public.jpeg"},
                "itemType":{"value":"public.jpeg"},"adjustmentRenderType":{"value":0}},"recordChangeTag":"recent-master"}),
            json!({"recordName":format!("asset-{master}"),"recordType":"CPLAsset","zoneID":zone,"fields":{
                "masterRef":{"value":{"recordName":master,"zoneID":zone},"type":"REFERENCE"},
                "assetDate":{"value":1_600_000_000_000i64 + number as i64*50_000_000_000,"type":"TIMESTAMP"},
                "addedDate":{"value":1_700_000_000_000i64,"type":"TIMESTAMP"}},"recordChangeTag":"recent-asset"}),
        ]
    }
}

impl Respond for RecentCliPhotos {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        use std::sync::atomic::Ordering;
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let path = request.url.path();
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let value = if path.ends_with("/changes/zone") {
            let continued =
                body.pointer("/zones/0/syncToken").and_then(Value::as_str) == Some("recent-page-1");
            let first = !continued && self.first_delta.swap(false, Ordering::SeqCst);
            let records = if first {
                self.pair(0)
            } else if continued {
                [self.pair(1), self.pair(2)].concat()
            } else {
                vec![]
            };
            json!({"zones":[{"zoneID":zone,"records":records,"syncToken":if first {"recent-page-1"}else{"recent-delta-successor"},"moreComing":first}]})
        } else if path.ends_with("/records/lookup") {
            let names: Vec<_> = body
                .get("records")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["recordName"].as_str().unwrap())
                .collect();
            let records: Vec<_> = (0..3)
                .flat_map(|n| self.pair(n))
                .filter(|r| names.contains(&r["recordName"].as_str().unwrap()))
                .collect();
            json!({"records":records})
        } else if path.ends_with("/internal/records/query/batch") {
            json!({"batch":[{"records":[{"fields":{"itemCount":{"value":3}}}]}]})
        } else if path.ends_with("/records/query")
            && body.pointer("/query/recordType").and_then(Value::as_str)
                == Some("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted")
        {
            self.queries.fetch_add(1, Ordering::SeqCst);
            let filters = body.pointer("/query/filterBy").unwrap().as_array().unwrap();
            let offset = filters
                .iter()
                .find(|f| f["fieldName"] == "startRank")
                .unwrap()
                .pointer("/fieldValue/value")
                .unwrap()
                .as_u64()
                .unwrap() as usize;
            let limit = body.get("resultsLimit").unwrap().as_u64().unwrap() as usize / 2;
            let records: Vec<_> = [2, 1, 0]
                .into_iter()
                .skip(offset)
                .take(limit)
                .flat_map(|n| self.pair(n))
                .collect();
            json!({"records":records,"syncToken":"recent-rank-token"})
        } else {
            return OfflinePhotos.respond(request);
        };
        ResponseTemplate::new(200).set_body_json(value)
    }
}

#[derive(Clone)]
struct PrivateSelectionCliPhotos {
    ranks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    rank_cycles: std::sync::Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    lookups: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    checks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl PrivateSelectionCliPhotos {
    fn pair(&self, number: usize) -> Vec<Value> {
        let mut pair = RecentCliPhotos {
            first_delta: Default::default(),
            queries: Default::default(),
        }
        .pair(number);
        let master = pair.get_mut(0).expect("released fixture has a master");
        *master
            .get_mut("recordName")
            .expect("master record identity") = json!(format!("private-{number}"));
        *master
            .pointer_mut("/fields/filenameEnc/value")
            .expect("master filename") = json!(format!("private-{number}.jpg"));
        let child = pair.get_mut(1).expect("released fixture has a child");
        *child.get_mut("recordName").expect("child record identity") =
            json!(format!("asset-private-{number}"));
        *child
            .pointer_mut("/fields/masterRef/value/recordName")
            .expect("child master relationship") = json!(format!("private-{number}"));
        pair
    }
    fn members(scope: Option<&str>) -> Vec<usize> {
        match scope {
            Some("A") => vec![0, 1],
            Some("B") => vec![0, 2],
            _ => vec![0, 1, 2, 3],
        }
    }
}
impl Respond for PrivateSelectionCliPhotos {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        use std::sync::atomic::Ordering;
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let path = request.url.path();
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let value = if path.ends_with("/zones/list") {
            json!({"zones":if path.contains("/private/"){vec![json!({"zoneID":zone})]}else{vec![]}})
        } else if body.pointer("/query/recordType").and_then(Value::as_str)
            == Some("CheckIndexingState")
        {
            json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]})
        } else if path.ends_with("/changes/database") {
            self.checks.fetch_add(1, Ordering::SeqCst);
            json!({"syncToken":"private-db-cursor","moreComing":false,"zones":[]})
        } else if path.ends_with("/changes/zone") {
            json!({"zones":[{"zoneID":zone,"records":[],"syncToken":"private-source-cursor","moreComing":false}]})
        } else if body.pointer("/query/recordType").and_then(Value::as_str)
            == Some("CPLAlbumByPositionLive")
        {
            let folders: Vec<_> = ["A", "B"]
                .into_iter()
                .map(|name| {
                    json!({
                        "recordName": name, "recordType": "CPLAlbum", "fields": {
                            "albumNameEnc": {"value": name, "type": "STRING"}
                        }
                    })
                })
                .collect();
            json!({"records": folders})
        } else if path.ends_with("/records/lookup") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let names: Vec<_> = body
                .get("records")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["recordName"].as_str().unwrap())
                .collect();
            json!({"records":(0..4).flat_map(|n|self.pair(n)).filter(|r|names.contains(&r["recordName"].as_str().unwrap())).collect::<Vec<_>>()})
        } else if path.ends_with("/internal/records/query/batch") {
            json!({"batch":body.get("batch").unwrap().as_array().unwrap().iter().map(|q|{
                let object=q.pointer("/query/filterBy/fieldValue/value/0").and_then(Value::as_str).unwrap();
                let scope=object.rsplit(':').next().unwrap();
                json!({"records":[{"fields":{"itemCount":{"value":Self::members(Some(scope)).len()}}}]})
            }).collect::<Vec<_>>()})
        } else if path.ends_with("/records/query") {
            let rank = self.ranks.fetch_add(1, Ordering::SeqCst) + 1;
            self.rank_cycles
                .lock()
                .unwrap()
                .push((self.checks.load(Ordering::SeqCst), rank));
            let filters = body
                .pointer("/query/filterBy")
                .and_then(Value::as_array)
                .unwrap();
            let field = |name: &str| {
                filters
                    .iter()
                    .find(|f| f["fieldName"] == name)
                    .and_then(|f| f.pointer("/fieldValue/value"))
            };
            let scope = field("parentId").and_then(Value::as_str);
            let offset = field("startRank").and_then(Value::as_u64).unwrap_or(0) as usize;
            let limit =
                usize::try_from(body.get("resultsLimit").unwrap().as_u64().unwrap()).unwrap() / 2;
            json!({"records":Self::members(scope).into_iter().skip(offset).take(limit).flat_map(|n|self.pair(n)).collect::<Vec<_>>(),"syncToken":"private-observational-rank"})
        } else {
            panic!("unexpected private selection CLI endpoint: {path}")
        };
        let response = ResponseTemplate::new(200).set_body_json(value);
        if path.ends_with("/records/query") && self.checks.load(Ordering::SeqCst) == 1 {
            // Synthetic first-cycle delay makes an early debt-only baseline
            // observable; later watch phases retain their normal timing.
            response.set_delay(Duration::from_millis(300))
        } else {
            response
        }
    }
}

#[derive(Clone)]
struct DeletedBoundAlbumPhotos {
    photos: PrivateSelectionCliPhotos,
    mode: std::sync::Arc<std::sync::atomic::AtomicU8>,
    album_lookups: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    media_url: String,
}

impl Respond for DeletedBoundAlbumPhotos {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        use std::sync::atomic::Ordering;
        let mode = self.mode.load(Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let kind = body.pointer("/query/recordType").and_then(Value::as_str);
        if mode != 0 && mode != 5 && kind == Some("CPLAlbumByPositionLive") {
            return ResponseTemplate::new(200).set_body_json(if mode == 3 {
                json!({"serverErrorCode":"INCOMPLETE_DISCOVERY"})
            } else {
                json!({"records":[]})
            });
        }
        if kind == Some("CPLAlbumByPositionLive") {
            let nested = body
                .pointer("/query/filterBy/0/fieldValue/value")
                .and_then(Value::as_str)
                == Some("family-folder");
            let (id, name, album_type) = if nested {
                ("A", if mode == 5 { "Renamed" } else { "A" }, 0)
            } else {
                ("family-folder", "Family", 3)
            };
            let mut record = json!({"recordName":id,"recordType":"CPLAlbum","fields":{"albumType":{"value":album_type},"albumNameEnc":{"value":base64::engine::general_purpose::STANDARD.encode(name)}}});
            if nested {
                record
                    .get_mut("fields")
                    .and_then(Value::as_object_mut)
                    .unwrap()
                    .insert(
                        "parentId".into(),
                        json!({"value":"family-folder","type":"STRING"}),
                    );
            }
            return ResponseTemplate::new(200).set_body_json(json!({"records":[record]}));
        }
        if request.url.path().ends_with("/records/lookup")
            && body
                .pointer("/records/0/recordName")
                .and_then(Value::as_str)
                == Some("A")
        {
            self.album_lookups.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                body.get("zoneID"),
                Some(&json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}))
            );
            let record = match mode {
                2 => json!({"recordName":"A","recordType":"CPLAlbum","deleted":true}),
                4 => json!({"recordName":"A","recordType":"CPLAlbum"}),
                _ => json!({"recordName":"A","serverErrorCode":"UNKNOWN_ITEM"}),
            };
            return ResponseTemplate::new(200).set_body_json(json!({"records":[record]}));
        }
        let pair = |number| {
            let mut records = self.photos.pair(number);
            *records
                .first_mut()
                .unwrap()
                .pointer_mut("/fields/resOriginalRes/value/downloadURL")
                .unwrap() = json!(self.media_url);
            records
        };
        if request.url.path().ends_with("/records/lookup") {
            let names: Vec<_> = body
                .get("records")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|record| record["recordName"].as_str().unwrap())
                .collect();
            return ResponseTemplate::new(200).set_body_json(json!({"records":(0..4).flat_map(pair).filter(|record|names.contains(&record["recordName"].as_str().unwrap())).collect::<Vec<_>>()}));
        }
        if request.url.path().ends_with("/records/query") && kind != Some("CheckIndexingState") {
            let filters = body
                .pointer("/query/filterBy")
                .and_then(Value::as_array)
                .unwrap();
            let field = |name: &str| {
                filters
                    .iter()
                    .find(|filter| filter["fieldName"] == name)
                    .and_then(|filter| filter.pointer("/fieldValue/value"))
            };
            let scope = field("parentId").and_then(Value::as_str);
            let offset = field("startRank").and_then(Value::as_u64).unwrap_or(0) as usize;
            let limit = body.get("resultsLimit").and_then(Value::as_u64).unwrap() as usize / 2;
            self.photos.ranks.fetch_add(1, Ordering::SeqCst);
            return ResponseTemplate::new(200).set_body_json(json!({"records":PrivateSelectionCliPhotos::members(scope).into_iter().skip(offset).take(limit).flat_map(pair).collect::<Vec<_>>(),"syncToken":"private-observational-rank"}));
        }
        self.photos.respond(request)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated HTTPS media proxy and test CA via KEI_TEST_HTTPS_PROXY and KEI_TEST_CA"]
async fn renamed_bound_album_actual_cli_preserves_user_tree_and_native_authority() {
    use std::sync::atomic::Ordering;
    fn snapshot_tree(root: &Path) -> Vec<(PathBuf, bool, Vec<u8>, std::time::SystemTime)> {
        let mut result = Vec::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            let path = entry.path();
            let bytes = if metadata.is_dir() {
                Vec::new()
            } else {
                std::fs::read(&path).unwrap()
            };
            result.push((
                path.clone(),
                metadata.is_dir(),
                bytes,
                metadata.modified().unwrap(),
            ));
            if metadata.is_dir() {
                result.extend(snapshot_tree(&path));
            }
        }
        result.sort_by(|left, right| left.0.cmp(&right.0));
        result
    }
    let proxy = std::env::var("KEI_TEST_HTTPS_PROXY").unwrap();
    let ca = std::env::var("KEI_TEST_CA").unwrap();
    let server = MockServer::start().await;
    let photos = DeletedBoundAlbumPhotos {
        photos: PrivateSelectionCliPhotos {
            ranks: Default::default(),
            rank_cycles: Default::default(),
            lookups: Default::default(),
            checks: Default::default(),
        },
        mode: Default::default(),
        album_lookups: Default::default(),
        media_url: "https://p01.icloud-content.com/media".into(),
    };
    Mock::given(method("POST"))
        .respond_with(photos.clone())
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    let conn = Connection::open(&account.path).unwrap();
    conn.execute_batch("UPDATE assets SET library='OldUnknownScope' WHERE id='retry'; DELETE FROM metadata WHERE key='pending_sync_token:old:PrimarySync';").unwrap();
    drop(conn);
    let config = std::fs::read_to_string(&account.config).unwrap().replace("[ui]", "folder_structure=\"\"\nfolder_structure_albums=\"{album}\"\n[filters]\nalbums=[\"Family/A\"]\nunfiled=false\n[ui]");
    std::fs::write(&account.config, &config).unwrap();
    let command = |args: &[&str]| {
        clean_cmd()
            .env("KEI_DATA_DIR", root)
            .env("ICLOUD_USERNAME", account.username)
            .env("HTTPS_PROXY", &proxy)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("SSL_CERT_FILE", &ca)
            .args(["--config", account.config.to_str().unwrap()])
            .args(args)
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap()
    };
    success(&command(&["sync", "--no-progress-bar"]));
    let media = account.media.parent().unwrap();
    let old = media.join("Family/A");
    std::fs::write(old.join("user.txt"), b"retain user entry").unwrap();
    photos.mode.store(5, Ordering::SeqCst);
    std::fs::write(
        &account.config,
        config.replace("Family/A", "Family/Renamed"),
    )
    .unwrap();
    let mut stable = None;
    let writable_config = std::fs::read_to_string(&account.config).unwrap();
    let pid = root.join("preview.pid");
    std::fs::write(&pid, b"retained invalid pid").unwrap();
    let preview_config = format!(
        "{writable_config}\n[watch]\npid_file={}\n",
        toml::Value::String(pid.to_string_lossy().into_owned())
    );
    #[cfg(unix)]
    let preview_config = {
        let mut preview_config = preview_config;
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("preview-notification.sh");
        let marker = root.join("preview-notification-fired");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf fired > '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        preview_config.push_str(&format!(
            "[notifications]\nscript={}\n",
            toml::Value::String(script.to_string_lossy().into_owned())
        ));
        preview_config
    };
    std::fs::write(&account.config, preview_config).unwrap();
    let before_preview = snapshot_tree(root);
    for flag in ["--dry-run", "--only-print-filenames"] {
        for prefix in [vec!["sync"], vec!["service", "run"]] {
            let mut args = prefix;
            args.extend([flag, "--no-progress-bar"]);
            let output = command(&args);
            success(&output);
            if flag == "--only-print-filenames" {
                let text = String::from_utf8_lossy(&output.stdout).replace('\\', "/");
                assert!(text.contains("Family/Renamed/private-0.JPG"), "{text}");
                assert!(text.contains("Family/Renamed/private-1.JPG"), "{text}");
            }
            assert_eq!(
                snapshot_tree(root),
                before_preview,
                "{flag} changed local state"
            );
        }
    }
    std::fs::write(&account.config, writable_config).unwrap();
    for _ in 0..3 {
        success(&command(&["sync", "--no-progress-bar"]));
        assert!(!old.exists());
        let renamed = media.join("Family/Renamed");
        assert_eq!(
            std::fs::read(renamed.join("user.txt")).unwrap(),
            b"retain user entry"
        );
        for number in 0..2 {
            assert_eq!(
                std::fs::read(renamed.join(format!("private-{number}.JPG"))).unwrap(),
                QUIET_MEDIA
            );
        }
        let conn = Connection::open(&account.path).unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM provider_active_destinations WHERE current_publication=1 AND replace(compat_path,char(92),'/') LIKE '%/Renamed/%'", [], |row| row.get(0)).unwrap(), 2);
        assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM provider_active_destinations WHERE current_publication=1 AND replace(compat_path,char(92),'/') LIKE '%/A/%'", [], |row| row.get(0)).unwrap(), 0);
        let snapshot = (
            rows(
                &account.path,
                "SELECT * FROM primary_layout_operations ORDER BY operation",
            ),
            rows(
                &account.path,
                "SELECT * FROM primary_layout_members ORDER BY operation,member",
            ),
            photos.photos.ranks.load(Ordering::SeqCst),
        );
        if let Some(previous) = &stable {
            assert_eq!(&snapshot, previous);
        }
        stable = Some(snapshot);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated HTTPS media proxy and test CA via KEI_TEST_HTTPS_PROXY and KEI_TEST_CA"]
async fn deleted_bound_album_actual_cli_redistributes_real_publications() {
    use std::sync::atomic::Ordering;
    let proxy = std::env::var("KEI_TEST_HTTPS_PROXY").unwrap();
    let ca = std::env::var("KEI_TEST_CA").unwrap();
    let command = |root: &Path, account: &Account, args: &[&str]| {
        clean_cmd()
            .env("KEI_DATA_DIR", root)
            .env("ICLOUD_USERNAME", account.username)
            .env("HTTPS_PROXY", &proxy)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("SSL_CERT_FILE", &ca)
            .args(["--config", account.config.to_str().unwrap()])
            .args(args)
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap()
    };
    for unfiled in [true, false] {
        let server = MockServer::start().await;
        let photos = DeletedBoundAlbumPhotos {
            photos: PrivateSelectionCliPhotos {
                ranks: Default::default(),
                rank_cycles: Default::default(),
                lookups: Default::default(),
                checks: Default::default(),
            },
            mode: Default::default(),
            album_lookups: Default::default(),
            media_url: "https://p01.icloud-content.com/media".into(),
        };
        Mock::given(method("POST"))
            .respond_with(photos.clone())
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let account = seed(root, ACCOUNTS[0], "A", &server.uri());
        let media_root = account.media.parent().unwrap();
        let conn = Connection::open(&account.path).unwrap();
        conn.execute_batch("UPDATE assets SET library='OldUnknownScope' WHERE id='retry'; DELETE FROM metadata WHERE key='pending_sync_token:old:PrimarySync';").unwrap();
        drop(conn);
        let config = std::fs::read_to_string(&account.config).unwrap().replace("[ui]",&format!("folder_structure=\"\"\nfolder_structure_albums=\"{{album}}\"\n[filters]\nalbums=[\"Family/A\"]\nunfiled={unfiled}\n[ui]"));
        std::fs::write(&account.config, &config).unwrap();
        success(&command(root, &account, &["sync", "--no-progress-bar"]));
        let old = media_root.join("Family/A/private-0.JPG");
        let kept = media_root.join("Family/A/private-1.JPG");
        assert_eq!(std::fs::read(&old).unwrap(), QUIET_MEDIA);
        assert_eq!(std::fs::read(&kept).unwrap(), QUIET_MEDIA);
        assert!(!media_root.join("private-0.JPG").exists());
        let conn = Connection::open(&account.path).unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1 AND current_publication=1 AND replace(compat_path,char(92),'/') LIKE '%/A/%'",[],|row|row.get(0)).unwrap(),2);
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM primary_layout_operations",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            0
        );
        assert!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_generations,json_each(CAST(specification AS TEXT),'$.profile.passes') WHERE json_extract(json_each.value,'$.selector_components[0]')='Family' AND json_extract(json_each.value,'$.selector_components[1]')='A'",[],|row|row.get(0)).unwrap()>0);
        drop(conn);
        std::fs::write(kept.with_extension("JPG.xmp"), b"foreign sidecar").unwrap();
        for mode in [1, 4] {
            photos.mode.store(mode, Ordering::SeqCst);
            assert!(
                !command(root, &account, &["sync", "--no-progress-bar"])
                    .status
                    .success()
            );
            assert_eq!(std::fs::read(&old).unwrap(), QUIET_MEDIA);
        }
        let checkpoint = rows(
            &account.path,
            "SELECT key,value FROM metadata WHERE key='sync_token:PrimarySync'",
        );
        photos.mode.store(2, Ordering::SeqCst);
        success(&command(root, &account, &["sync", "--no-progress-bar"]));
        assert!(!old.exists());
        assert_eq!(std::fs::read(&kept).unwrap(), QUIET_MEDIA);
        assert_eq!(
            std::fs::read(kept.with_extension("JPG.xmp")).unwrap(),
            b"foreign sidecar"
        );
        if unfiled {
            let conn = Connection::open(&account.path).unwrap();
            for number in 0..2 {
                let path = media_root.join(format!("private-{number}.JPG"));
                assert_eq!(std::fs::read(&path).unwrap(), QUIET_MEDIA);
                assert!(conn.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE compat_path=?1 AND admitted=1 AND verified_media=1 AND current_publication=1)",[path.to_str().unwrap()],|row|row.get(0)).unwrap());
            }
        } else {
            assert!(!media_root.join("private-0.JPG").exists());
            assert_eq!(
                rows(
                    &account.path,
                    "SELECT key,value FROM metadata WHERE key='sync_token:PrimarySync'"
                ),
                checkpoint
            );
        }
        let operations = rows(
            &account.path,
            "SELECT * FROM primary_layout_operations ORDER BY operation",
        );
        let probes = photos.album_lookups.load(Ordering::SeqCst);
        let ranks = photos.photos.ranks.load(Ordering::SeqCst);
        for _ in 0..2 {
            success(&command(root, &account, &["sync", "--no-progress-bar"]));
            assert_eq!(
                rows(
                    &account.path,
                    "SELECT * FROM primary_layout_operations ORDER BY operation"
                ),
                operations
            );
            assert_eq!(photos.album_lookups.load(Ordering::SeqCst), probes);
            assert_eq!(photos.photos.ranks.load(Ordering::SeqCst), ranks);
            if !unfiled {
                assert_eq!(
                    rows(
                        &account.path,
                        "SELECT key,value FROM metadata WHERE key='sync_token:PrimarySync'"
                    ),
                    checkpoint
                );
            }
        }
        photos.mode.store(3, Ordering::SeqCst);
        assert!(
            !command(root, &account, &["sync", "--no-progress-bar"])
                .status
                .success()
        );
        photos.mode.store(2, Ordering::SeqCst);
        std::fs::write(
            &account.config,
            config.replace("albums=[\"Family/A\"]", "albums=[\"NeverBound\"]"),
        )
        .unwrap();
        assert!(
            !command(root, &account, &["sync", "--no-progress-bar"])
                .status
                .success()
        );
        assert_eq!(photos.album_lookups.load(Ordering::SeqCst), probes);
    }
}
