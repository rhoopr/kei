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

async fn migration_command(root: &Path, account: &Account, args: &[&str]) {
    if args != ["service", "run"] {
        success(&command(root, account, args));
        return;
    }
    // Ordinary service execution now reaches watch mode. Stop only after its
    // first cycle, retaining the real startup/migration and reopen coverage.
    let log = root.join("migration-service.log");
    let mut child = QuietServiceChild(
        std::process::Command::new(assert_cmd::cargo::cargo_bin!("kei"))
            .env_clear()
            .envs(std::env::var_os("SystemRoot").map(|value| ("SystemRoot", value)))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root)
            .env("KEI_DATA_DIR", root)
            .env("ICLOUD_USERNAME", account.username)
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args(["--config", account.config.to_str().unwrap()])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let started = std::time::Instant::now();
    loop {
        let diagnostics = std::fs::read_to_string(&log).unwrap();
        if diagnostics.contains("Waiting before next cycle") {
            break;
        }
        assert!(child.0.try_wait().unwrap().is_none(), "{diagnostics}");
        assert!(started.elapsed() < Duration::from_secs(30), "{diagnostics}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    child.0.kill().unwrap();
    child.0.wait().unwrap();
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

/// This release's temporary bundles, complete or interrupted. A successful
/// verified upgrade removes its own; failures and prior releases leave theirs.
#[cfg(any(unix, windows))]
fn temporary_backup_bundles(root: &Path) -> Vec<PathBuf> {
    let mut bundles = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with(".kei-schema-backup-tmp-")
            })
        })
        .collect::<Vec<_>>();
    bundles.sort();
    bundles
}

/// Seed an older durable recovery bundle. The new policy must never discover,
/// modify, or delete bundles it did not create for this upgrade.
#[cfg(any(unix, windows))]
fn seed_prior_backup(root: &Path) -> PathBuf {
    let bundle = root.join(".kei-schema-backup-prior-evidence");
    std::fs::create_dir(&bundle).unwrap();
    std::fs::write(bundle.join("manifest.json"), br#"{"format":1}"#).unwrap();
    std::fs::write(bundle.join("state.db"), b"prior recovery database").unwrap();
    std::fs::write(bundle.join("restore.txt"), b"prior restore instructions").unwrap();
    bundle
}

#[cfg(any(unix, windows))]
fn bundle_files(bundle: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = std::fs::read_dir(bundle)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

#[cfg(any(unix, windows))]
fn file_digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

#[cfg(any(unix, windows))]
fn assert_verification_failure(args: &[&str], out: &std::process::Output) {
    let message = "Could not verify the upgraded state database";
    if args.first() == Some(&"doctor") {
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        let state = report
            .get("checks")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|check| check.get("name").and_then(Value::as_str) == Some("state_db"))
            .unwrap();
        assert_eq!(
            state.get("status").and_then(Value::as_str),
            Some("error"),
            "{args:?}: {state}"
        );
        assert!(
            state
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains(message)
                    || (text.starts_with("state DB read failed at ")
                        && text.ends_with(": <redacted>"))),
            "{args:?}: {state}"
        );
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}: {stderr}");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
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

#[cfg(any(unix, windows))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_backup_every_writable_cli_path_removes_its_own_bundle_and_keeps_prior_evidence()
{
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(OfflinePhotos)
        .mount(&server)
        .await;
    // `sync` and `service run` read the previous state file read-only to
    // build sync-core state and never migrate it, so the file itself stays
    // the rollback point; `sync_core::upgrade` tests cover that path.
    let paths: &[&[&str]] = &[
        &["status", "--downloaded", "--pending"],
        &["verify"],
        &["reconcile"],
        &["doctor", "--json"],
        &["import-existing", "--force-empty"],
        &["reset", "sync-token", "--yes"],
    ];
    for args in paths {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let account = seed(root, ACCOUNTS[0], "A", &server.uri());
        let mut config_bytes = std::fs::read(&account.config).unwrap();
        if *args == ["service", "run"] {
            config_bytes.extend_from_slice(b"\n[server]\nport=0\n");
        }
        config_bytes.extend_from_slice(b"\r\n# exact private input, not a regenerated config\r\n");
        std::fs::write(&account.config, &config_bytes).unwrap();
        let conn = Connection::open(&account.path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        // Remove precisely schema 36's additions; this is a populated old
        // application schema, not a version bump added for the backup feature.
        conn.execute_batch("DROP TABLE provider_transport_aliases; DROP TABLE provider_transport_canonical;
            PRAGMA user_version=35;
            CREATE TABLE future_unknown(payload BLOB); INSERT INTO future_unknown VALUES (X'00FF07');").unwrap();
        assert!(
            std::fs::metadata(account.path.with_extension("db-wal"))
                .unwrap()
                .len()
                > 0
        );
        // An unknown child of a missing parent commits with foreign-key
        // enforcement off, then fails post-commit verification. The retained
        // bundle is the exact pre-command state and loaded config.
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             CREATE TABLE verify_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE verify_child(id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES verify_parent(id));
             INSERT INTO verify_child(id,parent_id) VALUES (1, 99);
             PRAGMA foreign_keys=ON;",
        )
        .unwrap();
        let before = state(&account.path);
        let prior = seed_prior_backup(root);
        let prior_files = bundle_files(&prior);
        let failed = command(root, &account, args);
        assert_verification_failure(args, &failed);
        assert_eq!(
            conn.query_row::<i32, _, _>("PRAGMA user_version", [], |row| row.get(0))
                .unwrap(),
            super::HELPER_SCHEMA_VERSION,
            "{args:?}: the migration commits before verification"
        );
        let (saved, incomplete) = migration_backup_bundles(root);
        assert!(
            incomplete.is_empty(),
            "{args:?}: incomplete bundles: {incomplete:?}"
        );
        let retained = temporary_backup_bundles(root);
        assert_eq!(
            retained.len(),
            1,
            "{args:?}: verification failure retains its exact snapshot"
        );
        let retained = retained[0].clone();
        let mut expected = vec![prior.clone(), retained.clone()];
        expected.sort();
        assert_eq!(saved, expected);
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(retained.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["schema_version"], 35);
        assert_eq!(manifest["config"]["kind"], "file");
        assert_eq!(
            std::fs::read(retained.join("config.toml")).unwrap(),
            config_bytes,
            "{args:?}: exact loaded --config source"
        );
        assert_eq!(
            manifest["config"]["sha256"],
            file_digest(&retained.join("config.toml"))
        );
        assert_eq!(
            manifest["database_sha256"],
            file_digest(&retained.join("state.db"))
        );
        #[cfg(unix)]
        assert_eq!(manifest["native_path_encoding"], "unix_bytes_hex");
        #[cfg(windows)]
        assert_eq!(manifest["native_path_encoding"], "windows_utf16le_hex");
        assert!(!retained.join("state.db-wal").exists());
        {
            let snapshot = Connection::open_with_flags(
                retained.join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            assert_eq!(
                snapshot
                    .query_row::<i32, _, _>("PRAGMA user_version", [], |row| row.get(0))
                    .unwrap(),
                35
            );
            assert_eq!(
                snapshot
                    .query_row::<Vec<u8>, _, _>("SELECT payload FROM future_unknown", [], |row| row
                        .get(0))
                    .unwrap(),
                [0, 255, 7]
            );
        }
        assert_eq!(
            state(&retained.join("state.db")),
            before,
            "{args:?}: exact pre-command receipts, history and WAL content"
        );
        // Remove only the injected verification fault and rerun the same route.
        Connection::open(&account.path)
            .unwrap()
            .execute_batch("DROP TABLE verify_child; DROP TABLE verify_parent;")
            .unwrap();
        migration_command(root, &account, args).await;
        assert!(
            temporary_backup_bundles(root).is_empty(),
            "{args:?}: the verified retry removes its own temporary bundle"
        );
        assert!(!retained.exists());
        let (reopened, incomplete) = migration_backup_bundles(root);
        assert!(
            incomplete.is_empty(),
            "{args:?}: incomplete bundles: {incomplete:?}"
        );
        assert_eq!(
            reopened,
            std::slice::from_ref(&prior),
            "{args:?}: only the prior recovery bundle remains"
        );
        assert_eq!(
            bundle_files(&prior),
            prior_files,
            "{args:?}: prior recovery evidence must stay untouched"
        );
        assert_eq!(
            std::fs::read(&account.config).unwrap(),
            config_bytes,
            "{args:?}: the exact --config source is unchanged"
        );
        assert_eq!(
            std::fs::read(&account.media).unwrap(),
            b"known media belonging only to account A"
        );
        for _ in 0..2 {
            migration_command(root, &account, args).await;
            assert!(temporary_backup_bundles(root).is_empty());
            let (reopened, incomplete) = migration_backup_bundles(root);
            assert!(
                incomplete.is_empty(),
                "{args:?}: incomplete bundles: {incomplete:?}"
            );
            assert_eq!(
                reopened,
                std::slice::from_ref(&prior),
                "{args:?}: quiet reopens neither back up again nor touch prior evidence"
            );
            assert_eq!(bundle_files(&prior), prior_files);
        }
    }
}

#[cfg(any(unix, windows))]
#[test]
fn migration_backup_no_file_context_stays_absent_until_retained_state_is_backed_up() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", "http://127.0.0.1:1");
    let conn = Connection::open(&account.path).unwrap();
    // Fail the schema-44 deletion-check migration with a conflicting fixture
    // table so the startup bundle is retained for exact inspection.
    conn.execute_batch("DROP TABLE provider_transport_aliases; DROP TABLE provider_transport_canonical;
        DROP TRIGGER IF EXISTS historical_deletion_observed; DROP TRIGGER IF EXISTS historical_deletion_inserted;
        DROP INDEX IF EXISTS historical_deletion_due; DROP TABLE historical_deletion_checks;
        PRAGMA user_version=35; CREATE TABLE historical_deletion_checks(fake TEXT);").unwrap();
    std::fs::remove_file(&account.config).unwrap();
    let original = std::fs::read(&account.path).unwrap();
    let refused = command(root, &account, &["sync"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Set [download].directory"));
    assert!(
        !account.config.exists(),
        "retained state must not acquire an auto-config before backup"
    );
    assert_eq!(std::fs::read(&account.path).unwrap(), original);
    let (saved, incomplete) = migration_backup_bundles(root);
    assert!(saved.is_empty() && incomplete.is_empty());
    // A failed migration never records the retirement receipt, so its complete
    // bundle remains as the exact pre-upgrade config-absence evidence.
    let failed = command(root, &account, &["status"]);
    assert!(!failed.status.success());
    let (saved, incomplete) = migration_backup_bundles(root);
    assert!(incomplete.is_empty(), "incomplete bundles: {incomplete:?}");
    assert_eq!(saved.len(), 1);
    let bundle = saved[0].clone();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["config"]["kind"], "absent");
    assert!(!bundle.join("config.toml").exists());
    assert!(!account.config.exists());
    let failed_files = bundle_files(&bundle);
    // Remove only the injected conflict. The next successful upgrade removes
    // its own bundle and leaves the failed one untouched.
    Connection::open(&account.path)
        .unwrap()
        .execute_batch("DROP TABLE historical_deletion_checks")
        .unwrap();
    success(&command(root, &account, &["status"]));
    let (retained, incomplete) = migration_backup_bundles(root);
    assert_eq!(retained, std::slice::from_ref(&bundle));
    assert!(incomplete.is_empty(), "incomplete bundles: {incomplete:?}");
    assert_eq!(bundle_files(&bundle), failed_files);
    assert_eq!(
        temporary_backup_bundles(root),
        std::slice::from_ref(&bundle)
    );
    assert!(!account.config.exists());
    for _ in 0..2 {
        success(&command(root, &account, &["status"]));
        let (reopened, incomplete) = migration_backup_bundles(root);
        assert_eq!(reopened, std::slice::from_ref(&bundle));
        assert!(incomplete.is_empty(), "incomplete bundles: {incomplete:?}");
        assert_eq!(bundle_files(&bundle), failed_files);
        assert!(!account.config.exists());
    }
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

#[derive(Clone)]
struct UnknownChanges;
impl Respond for UnknownChanges {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if request.url.path().ends_with("/changes/zone") {
            ResponseTemplate::new(200).set_body_json(json!({"zones":[{
                "zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                "syncToken":"A-cursor","moreComing":false,
                "records":[{"recordName":"future-source","recordType":"FutureRecord",
                    "futureEnvelope":{"relationship":{"recordName":"unresolved-peer"}}}]
            }]}))
        } else {
            OfflinePhotos.respond(request)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_actual_sync_captures_unknown_source_after_library_resolution() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(UnknownChanges)
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    success(&command(root, &account, &["sync", "--no-progress-bar"]));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path().ends_with("/changes/zone")),
        "production sync must observe an incremental page"
    );
    let conn =
        Connection::open_with_flags(&account.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    // The startup path, not a manually attached test album, must capture the
    // unknown record before the existing stream drops its lossy projection.
    let records: i64 = conn
        .query_row(
            "SELECT count(*) FROM provider_shadow_records WHERE record_name='future-source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        records, 1,
        "resolved library clones must carry capture composition"
    );
    let body: Vec<u8> = conn
        .query_row("SELECT body FROM provider_shadow_pages", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("unresolved-peer"));
    assert_eq!(
        std::fs::read(&account.media).unwrap(),
        b"known media belonging only to account A"
    );
}

#[derive(Clone)]
struct ShadowHoldProvider {
    failure: std::sync::Arc<std::sync::atomic::AtomicBool>,
    kind: &'static str,
}
impl Respond for ShadowHoldProvider {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if request.url.path().ends_with("/changes/zone") {
            let cursor = body.pointer("/zones/0/syncToken").and_then(Value::as_str);
            // Empty polls need no inbox space. Offer a second distinct record
            // at the saved cursor to exercise refusal and retry at capacity.
            let capacity_page = self.kind == "capacity" && cursor == Some("delta-successor");
            let quiet = cursor == Some("capacity-successor")
                || (cursor == Some("delta-successor") && !capacity_page);
            let mut response = json!({"zones":[{
                "zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                "syncToken":if self.kind == "capacity" && (capacity_page || quiet) { "capacity-successor" } else { "delta-successor" },"moreComing":false,
                "records":if quiet { json!([]) } else { json!([{
                    "recordName":if capacity_page { "second-source" } else { "future-source" },"recordType":"FutureRecord",
                    "futureEnvelope":{"relationship":{"recordName":"unresolved-peer"}}
                }]) }
            }]});
            if self.failure.load(std::sync::atomic::Ordering::SeqCst) && !quiet {
                if self.kind == "identity" {
                    *response
                        .pointer_mut("/zones/0/records/0/recordName")
                        .expect("known source fixture") = json!("");
                } else if self.kind == "json" {
                    return ResponseTemplate::new(200).set_body_string(
                        response.to_string().replace(
                            "\"recordName\":\"future-source\"",
                            "\"recordName\":\"private-other\",\"recordName\":\"future-source\"",
                        ),
                    );
                }
            }
            ResponseTemplate::new(200).set_body_json(response)
        } else if request.url.path().ends_with("/records/query")
            && body
                .pointer("/query/recordType")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.starts_with("CPLAsset"))
        {
            // A valid rank EOF/new anchor is deliberately available. It cannot
            // stand in for an incremental observation that was refused.
            ResponseTemplate::new(200).set_body_json(json!({
                "records":[],"syncToken":"inventory-after-uncaptured-page"
            }))
        } else {
            OfflinePhotos.respond(request)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_actual_cli_refusals_hold_without_rank_fallback_then_recover_quietly() {
    fn selected_ranks(requests: &[Request]) -> Vec<Value> {
        requests
            .iter()
            .filter_map(|request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                (request.url.path().ends_with("/records/query")
                    && body["query"]["recordType"]
                        .as_str()
                        .is_some_and(|kind| kind.starts_with("CPLAsset")))
                .then_some(body)
            })
            .collect()
    }

    fn assert_empty_inventory(ranks: &[Value], phase: &str) {
        // The rank API has no EOF flag: one empty page plus four forward
        // probes proves an empty inventory, not five inventory retries.
        assert_eq!(ranks.len(), 5, "{phase}: complete empty-tail proof");
        let first = &ranks[0];
        assert_eq!(
            first["query"]["recordType"],
            "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"
        );
        assert_eq!(
            first["zoneID"],
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})
        );
        let limit = first["resultsLimit"].as_u64().unwrap();
        assert!(limit > 0 && limit.is_multiple_of(2));
        for (index, rank) in ranks.iter().enumerate() {
            let mut expected = first.clone();
            let start = expected["query"]["filterBy"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|filter| filter["fieldName"] == "startRank")
                .unwrap();
            start["fieldValue"]["value"] = json!(index as u64 * (limit / 2));
            assert_eq!(
                *rank, expected,
                "{phase}: only the rank advances between EOF probes"
            );
        }
    }

    for (kind, expected) in [
        ("write", "synthetic CLI receipt failure"),
        ("capacity", "inbox is full"),
        ("identity", "Missing provider capture source identity"),
        ("json", "Invalid or ambiguous provider changes JSON"),
    ] {
        let failure = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ShadowHoldProvider {
                failure: failure.clone(),
                kind,
            })
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let account = seed(root, ACCOUNTS[0], "A", &server.uri());
        {
            let conn = Connection::open(&account.path).unwrap();
            // Remove fixture-only debt before establishing initial state so it
            // cannot mask an otherwise eligible rank checkpoint.
            conn.execute_batch("DELETE FROM assets WHERE id='retry'; DELETE FROM metadata WHERE key LIKE 'pending_sync_token:%';").unwrap();
            if kind == "write" {
                conn.execute_batch("CREATE TRIGGER shadow_cli_fault AFTER INSERT ON provider_shadow_pages BEGIN SELECT RAISE(ABORT,'synthetic CLI receipt failure'); END;").unwrap();
            }
        }
        let original_charge = if kind == "capacity" {
            // Start from a real captured page with usable provenance. Inject
            // logical charge exhaustion without a 512 MiB physical fixture.
            success(&command(root, &account, &["sync", "--no-progress-bar"]));
            let conn = Connection::open(&account.path).unwrap();
            let charge: i64 = conn
                .query_row(
                    "SELECT charged_bytes FROM provider_shadow_pages",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            conn.execute(
                "UPDATE provider_shadow_pages SET charged_bytes=536870912",
                [],
            )
            .unwrap();
            Some(charge)
        } else {
            None
        };
        let before = state(&account.path);
        let initial_pages = i64::from(kind == "capacity");
        let raw_before = rows(
            &account.path,
            "SELECT body_hash,body FROM provider_shadow_pages ORDER BY id",
        );
        for _ in 0..2 {
            let requests_before = server.received_requests().await.unwrap().len();
            let ranks_before = rows(
                &account.path,
                "SELECT id FROM provider_selection_rank_pages",
            );
            let out = command(root, &account, &["sync", "--no-progress-bar"]);
            let diagnostics = String::from_utf8_lossy(&out.stderr);
            assert!(diagnostics.contains(expected), "{kind}: {diagnostics}");
            assert!(
                !diagnostics.contains("falling back to full enumeration"),
                "{kind}: {diagnostics}"
            );
            assert!(!diagnostics.contains("private-other"));
            let requests = server.received_requests().await.unwrap();
            let ranks = selected_ranks(&requests[requests_before..]);
            // A newly qualified selection may need its initial empty rank
            // page. Capture refusal cannot retry it as a substitute source
            // checkpoint, or rescan an already captured current selection.
            if !ranks.is_empty() {
                assert!(
                    ranks_before.is_empty(),
                    "{kind}: capture refusal must not repeat a retained inventory; {diagnostics}"
                );
                assert_empty_inventory(&ranks, kind);
                assert_eq!(
                    rows(
                        &account.path,
                        "SELECT id FROM provider_selection_rank_pages"
                    )
                    .len(),
                    5,
                    "{kind}: initial rank must belong to the captured native selection"
                );
            }
            assert_eq!(
                state(&account.path),
                before,
                "{kind}: retained owned history/checkpoint"
            );
            assert_eq!(
                rows(
                    &account.path,
                    "SELECT body_hash,body FROM provider_shadow_pages ORDER BY id"
                ),
                raw_before,
                "{kind}: refusal must preserve exact captured source bytes"
            );
            let conn = Connection::open(&account.path).unwrap();
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                initial_pages,
                "{kind}"
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_records",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                initial_pages,
                "{kind}"
            );
            assert_eq!(
                std::fs::read(&account.media).unwrap(),
                b"known media belonging only to account A"
            );
        }
        {
            let conn = Connection::open(&account.path).unwrap();
            if kind == "write" {
                conn.execute_batch("DROP TRIGGER shadow_cli_fault").unwrap();
            } else if kind == "capacity" {
                conn.execute(
                    "UPDATE provider_shadow_pages SET charged_bytes=?1",
                    [original_charge.unwrap()],
                )
                .unwrap();
            }
        }
        failure.store(false, std::sync::atomic::Ordering::SeqCst);
        let recovery_requests = server.received_requests().await.unwrap().len();
        let selection_missing = rows(
            &account.path,
            "SELECT id FROM provider_selection_rank_pages",
        )
        .is_empty();
        success(&command(root, &account, &["sync", "--no-progress-bar"]));
        let requests = server.received_requests().await.unwrap();
        let recovery_ranks = selected_ranks(&requests[recovery_requests..]);
        // Admitting the second previously unseen capacity page also changes
        // the selection dependencies and requires one inventory revalidation.
        if selection_missing || kind == "capacity" {
            assert_empty_inventory(&recovery_ranks, kind);
        } else {
            assert!(
                recovery_ranks.is_empty(),
                "{kind}: recovery must reuse the captured inventory"
            );
        }
        for _ in 0..2 {
            // Actual CLI processes reopen independently; the second empty
            // page replay must not grow observations or repeat publication.
            let requests_before = server.received_requests().await.unwrap().len();
            success(&command(root, &account, &["sync", "--no-progress-bar"]));
            let requests = server.received_requests().await.unwrap();
            assert!(
                selected_ranks(&requests[requests_before..]).is_empty(),
                "{kind}: quiet reopen must reuse the completed selected rank"
            );
            let conn = Connection::open(&account.path).unwrap();
            assert_eq!(
                conn.query_row::<String, _, _>(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                if kind == "capacity" {
                    "capacity-successor"
                } else {
                    "delta-successor"
                },
                "{kind}"
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                1 + initial_pages, // Only nonempty pages remain.
                "{kind}"
            );
            assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_shadow_records WHERE record_name='future-source'",[],|row|row.get(0)).unwrap(), 1, "{kind}");
            assert_eq!(
                rows(&account.path, "SELECT * FROM asset_metadata_paths"),
                before[3]
            );
            assert_eq!(
                std::fs::read(&account.media).unwrap(),
                b"known media belonging only to account A"
            );
            assert_eq!(
                std::fs::read_dir(account.media.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_projection_actual_cli_replays_current_backlog_after_fault_and_stays_quiet() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(OfflinePhotos)
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    let raw=serde_json::to_vec(&json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
        "syncToken":"retained-source-successor","moreComing":false,"records":[
            {"recordName":"retained-master","recordType":"CPLMaster","fields":{"futureBits":{"value":[1,2,3]}}},
            {"recordName":"retained-child","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"retained-master"}}}},
            {"recordName":"retained-future","recordType":"FutureRecord","fields":{"peer":{"value":{"recordName":"external-peer","zoneID":{"zoneName":"external-zone","ownerRecordName":"external-owner"}}}}},
            {"recordName":"retained-tomb","deleted":true,"futureTombBytes":[5,9]}
        ]}]})).unwrap();
    let scope=serde_json::to_string(&json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"private","zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}})).unwrap();
    {
        let conn = Connection::open(&account.path).unwrap();
        // Keep durable retry/old-epoch obligations in an unrelated scope so a
        // quiet PrimarySync cycle does not independently process those rows.
        // The seed's pending_sync_token key is a config candidate, not debt;
        // replace it with actual unresolved source evidence for this fixture.
        conn.execute_batch("UPDATE assets SET library='OldUnknownScope' WHERE id='retry'; DELETE FROM metadata WHERE key='pending_sync_token:old:PrimarySync';").unwrap();
        let original =
            json!([1, "retained-original", "OldUnknownScope", "private-owner"]).to_string();
        let observed =
            json!([1, "retained-observed", "OldUnknownScope", "private-owner"]).to_string();
        conn.execute("INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at) VALUES('OldUnknownScope','historical-source',?1,?2,1,1700000000)",params![original,observed]).unwrap();
        let owner: (String, String) = conn
            .query_row(
                "SELECT account_key,provider_key FROM account_owner",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let identities = [
            ("retained-master", Some("CPLMaster"), false),
            ("retained-child", Some("CPLAsset"), false),
            ("retained-future", Some("FutureRecord"), false),
            ("retained-tomb", None, true),
        ];
        let charge = raw.len()
            + scope.len()
            + "A-cursor".len()
            + "retained-source-successor".len()
            + owner.0.len()
            + owner.1.len()
            + 128
            + identities
                .iter()
                .map(|(name, kind, _)| name.len() + kind.map_or(0, str::len) + 32)
                .sum::<usize>();
        conn.execute("INSERT INTO provider_shadow_pages(account_key,provider_key,scope,request_cursor,successor,more_coming,body_hash,body,charged_bytes,observed_at) VALUES(?1,?2,?3,'A-cursor','retained-source-successor',0,?4,?5,?6,1700000000)",params![owner.0,owner.1,scope,format!("{:x}",Sha256::digest(&raw)),raw,charge as i64]).unwrap();
        let id = conn.last_insert_rowid();
        for (ordinal, (name, kind, deleted)) in identities.iter().enumerate() {
            conn.execute("INSERT INTO provider_shadow_records(page_id,ordinal,record_name,record_type,deleted) VALUES(?1,?2,?3,?4,?5)",params![id,ordinal as i64,name,kind,deleted]).unwrap();
        }
        conn.execute(
            "INSERT INTO provider_shadow_receipts VALUES(?1,?2)",
            params![scope, id],
        )
        .unwrap();
        // Current captured observations remain executable and checkpoint-blocking.
        // Schema-38 retirement of historical observations is covered separately.
    }
    success(&command(root, &account, &["status"]));
    {
        let conn = Connection::open(&account.path).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            i64::from(super::support::HELPER_SCHEMA_VERSION)
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_catalog_records",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            0,
            "status migration must not invent indexing proof"
        );
        conn.execute_batch("CREATE TRIGGER catalog_cli_fault BEFORE INSERT ON provider_catalog_pages BEGIN SELECT RAISE(ABORT,'synthetic CLI catalog receipt failure'); END;").unwrap();
    }
    let before = state(&account.path);
    let debt_before = rows(
        &account.path,
        "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name",
    );
    for _ in 0..2 {
        let out = command(root, &account, &["sync", "--no-progress-bar"]);
        let diagnostics = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(
            diagnostics.contains("synthetic CLI catalog receipt failure"),
            "{diagnostics}"
        );
        assert!(!diagnostics.contains("falling back to full enumeration"));
        assert_eq!(state(&account.path), before);
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
            ),
            debt_before
        );
        assert_eq!(
            std::fs::read(&account.media).unwrap(),
            b"known media belonging only to account A"
        );
        let conn = Connection::open(&account.path).unwrap();
        for table in [
            "provider_catalog_records",
            "provider_catalog_references",
            "provider_catalog_debt",
            "provider_catalog_pages",
        ] {
            assert_eq!(
                conn.query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap(),
                0
            );
        }
        assert_eq!(
            conn.query_row::<Vec<u8>, _, _>("SELECT body FROM provider_shadow_pages", [], |row| {
                row.get(0)
            })
            .unwrap(),
            raw
        );
    }
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("/changes/zone")),
        "startup replay refusal must happen before live delta dispatch"
    );
    Connection::open(&account.path)
        .unwrap()
        .execute_batch("DROP TRIGGER catalog_cli_fault")
        .unwrap();
    let recover_with_retained_debt = || {
        let out = command(root, &account, &["sync", "--no-progress-bar"]);
        let diagnostics = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "unselected OldUnknownScope must not fail this sync: {diagnostics}"
        );
        assert!(
            !diagnostics.contains("synthetic CLI catalog receipt failure"),
            "{diagnostics}"
        );
        assert!(
            !diagnostics.contains("falling back to full enumeration"),
            "{diagnostics}"
        );
    };
    recover_with_retained_debt();
    let frozen = rows(
        &account.path,
        "SELECT * FROM provider_catalog_pages ORDER BY page_id",
    );
    for _ in 0..2 {
        recover_with_retained_debt();
        assert_eq!(state(&account.path), before);
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
            ),
            debt_before
        );
        let conn = Connection::open(&account.path).unwrap();
        for (table, count) in [
            ("provider_catalog_records", 4),
            ("provider_catalog_references", 2),
            ("provider_catalog_debt", 4),
            ("provider_catalog_pages", 1), // Only the retained backlog page.
        ] {
            assert_eq!(
                conn.query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap(),
                count,
                "{table}"
            );
        }
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM provider_catalog_pages ORDER BY page_id"
            ),
            frozen
        );
        assert_eq!(
            conn.query_row::<Vec<u8>, _, _>(
                "SELECT body FROM provider_shadow_pages ORDER BY id LIMIT 1",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            raw
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            "A-cursor",
            "retained observation successor never owns current checkpoint"
        );
        assert_eq!(
            std::fs::read(&account.media).unwrap(),
            b"known media belonging only to account A"
        );
        assert_eq!(
            std::fs::read_dir(account.media.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }
}

// A real service process, real 60-second watch cadence, synthetic account only.
// The receipt fault interrupts between durable capture and stream emission.
const QUIET_MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\x01\0\0\xff\xd9";

#[derive(Clone)]
struct QuietCatalogPhotos {
    capture_once: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tail_available: std::sync::Arc<std::sync::atomic::AtomicBool>,
    lookups: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    checks: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl QuietCatalogPhotos {
    fn pair(&self, number: usize) -> Vec<Value> {
        let master = format!("quiet-{number}");
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        vec![
            json!({"recordName":master,"recordType":"CPLMaster","zoneID":zone,"fields":{
            "filenameEnc":{"value":base64::engine::general_purpose::STANDARD.encode(format!("quiet-{number}.jpg")),"type":"STRING"},
            "resOriginalRes":{"value":{"downloadURL":"https://p01.icloud-content.com/synthetic-never-fetch","size":QUIET_MEDIA.len(),
                "fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(QUIET_MEDIA))}},
            "resOriginalWidth":{"value":1,"type":"INT64"},"resOriginalHeight":{"value":1,"type":"INT64"},
            "resOriginalFileType":{"value":"public.jpeg"},"itemType":{"value":"public.jpeg"},
            "adjustmentRenderType":{"value":0,"type":"INT64"}},"recordChangeTag":"master-current"}),
            json!({"recordName":format!("asset-{master}"),"recordType":"CPLAsset","zoneID":zone,"fields":{
                "masterRef":{"value":{"recordName":master,"zoneID":zone},"type":"REFERENCE"},
                "assetDate":{"value":1700000000000i64,"type":"TIMESTAMP"},
                "addedDate":{"value":1700000000000i64,"type":"TIMESTAMP"}},"recordChangeTag":"asset-current"}),
        ]
    }
}
impl Respond for QuietCatalogPhotos {
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
            json!({"syncToken":"quiet-database-cursor","moreComing":false,"zones":[]})
        } else if path.ends_with("/changes/zone") {
            let records = if self.capture_once.swap(false, Ordering::SeqCst) {
                let mut records = vec![
                    json!({"recordName":"asset-unresolved-first","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"unresolved-master","zoneID":zone}}}}),
                ];
                for number in 0..66 {
                    records.extend(self.pair(number));
                }
                records.push(json!({"recordName":"future-source","recordType":"FutureRecord","opaque":[1,2,3]}));
                records.push(json!({"recordName":"explicit-tomb","deleted":true,"opaque":[4,5]}));
                records
            } else {
                vec![]
            };
            json!({"zones":[{"zoneID":zone,"syncToken":"A-cursor","moreComing":false,"records":records}]})
        } else if path.ends_with("/records/lookup") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let mut records = Vec::new();
            for identity in body.get("records").and_then(Value::as_array).unwrap() {
                let name = identity.get("recordName").and_then(Value::as_str).unwrap();
                let master = name.strip_prefix("asset-").unwrap_or(name);
                if let Some(number) = master
                    .strip_prefix("quiet-")
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|number| {
                        *number < 66
                            || (*number == 66 && self.tail_available.load(Ordering::SeqCst))
                    })
                {
                    records.extend(
                        self.pair(number)
                            .into_iter()
                            .filter(|r| r.get("recordName").and_then(Value::as_str) == Some(name)),
                    );
                } else {
                    records.push(
                        json!({"recordName":name,"zoneID":zone,"serverErrorCode":"UNKNOWN_ITEM"}),
                    );
                }
            }
            json!({"records":records})
        } else if path.ends_with("/internal/records/query/batch") {
            json!({"batch":[{"records":[{"fields":{"itemCount":{"value":67}}}]}]})
        } else if body.pointer("/query/recordType").and_then(Value::as_str)
            == Some("CPLAlbumByPositionLive")
        {
            json!({"records":[]})
        } else if path.ends_with("/records/query") {
            assert_eq!(
                body.pointer("/query/recordType").and_then(Value::as_str),
                Some("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted")
            );
            assert_eq!(body.get("zoneID"), Some(&zone));
            let offset = body
                .pointer("/query/filterBy")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|filter| filter.get("fieldName").and_then(Value::as_str) == Some("startRank"))
                .and_then(|filter| filter.pointer("/fieldValue/value").and_then(Value::as_u64))
                .unwrap_or(0) as usize;
            let limit = body
                .get("resultsLimit")
                .and_then(Value::as_u64)
                .unwrap_or(200) as usize
                / 2;
            let records: Vec<_> = (0..67)
                .skip(offset)
                .take(limit)
                .flat_map(|number| self.pair(number))
                .collect();
            json!({"records":records,"syncToken":"A-rank-cursor"})
        } else {
            panic!("unexpected quiet catalog request: {path}")
        };
        ResponseTemplate::new(200).set_body_json(value)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_work_actual_service_replays_interrupted_capture_and_drains_quiet_watch_tail() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let server = MockServer::start().await;
    let photos = QuietCatalogPhotos {
        capture_once: Arc::new(AtomicBool::new(true)),
        tail_available: Arc::new(AtomicBool::new(false)),
        lookups: Arc::new(AtomicUsize::new(0)),
        checks: Arc::new(AtomicUsize::new(0)),
    };
    Mock::given(method("POST"))
        .respond_with(photos.clone())
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    // An upper date bound filters after native confirmation. A lower bound
    // ends enumeration before these older identities reach the scheduler.
    // Query all 67 identities, then independently classify their current work.
    let config = format!(
        "{}[filters]\nskip_created_after=\"2000-01-01\"\n",
        std::fs::read_to_string(&account.config).unwrap()
    );
    std::fs::write(&account.config, &config).unwrap();
    {
        let conn = Connection::open(&account.path).unwrap();
        conn.execute_batch("UPDATE assets SET library='OldUnknownScope' WHERE id='retry'; DELETE FROM metadata WHERE key='pending_sync_token:old:PrimarySync'; CREATE TRIGGER interrupt_after_capture BEFORE INSERT ON provider_catalog_pages BEGIN SELECT RAISE(ABORT,'synthetic interruption after capture'); END;").unwrap();
    }
    let before = state(&account.path);
    let out = command(root, &account, &["sync", "--no-progress-bar"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("synthetic interruption after capture"),
        "{:?}",
        out
    );
    assert_eq!(state(&account.path), before);
    assert_eq!(photos.lookups.load(Ordering::SeqCst), 0);
    let count = |sql: &str| {
        Connection::open(&account.path)
            .unwrap()
            .query_row::<i64, _, _>(sql, [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count("SELECT count(*) FROM provider_shadow_pages"), 1);
    assert_eq!(count("SELECT count(*) FROM provider_catalog_records"), 0);
    assert_eq!(count("SELECT count(*) FROM provider_work_receipts"), 0);
    assert_eq!(
        count("SELECT count(*) FROM provider_active_decisions"),
        0,
        "catalog replay refusal precedes native selected admission: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        count("SELECT count(*) FROM provider_selection_rank_pages"),
        0,
        "capture interruption must precede selected rank capture: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let captured = rows(
        &account.path,
        "SELECT * FROM provider_shadow_pages ORDER BY id",
    );
    Connection::open(&account.path)
        .unwrap()
        .execute_batch("DROP TRIGGER interrupt_after_capture")
        .unwrap();
    // Native scheduling classifies the exact current query, not a 64-row slice
    // of historical catalog pages. Its one unresolved identity must retain a
    // retry deadline while the 66 filtered peers remain independently settled.
    let mut config = std::fs::read_to_string(&account.config).unwrap();
    config.push_str("[watch]\ninterval=60\n[server]\nport=0\n");
    std::fs::write(&account.config, &config).unwrap();
    let log = root.join("quiet-service.log");
    let stderr = std::fs::File::create(&log).unwrap();
    let mut child = QuietServiceChild(
        std::process::Command::new(assert_cmd::cargo::cargo_bin!("kei"))
            .env_clear()
            // Preserve the Windows runtime root without inheriting account/auth
            // environment. The cleared service process still needs networking.
            .envs(std::env::var_os("SystemRoot").map(|value| ("SystemRoot", value)))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root)
            .env("KEI_DATA_DIR", root)
            .env("ICLOUD_USERNAME", account.username)
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args([
                "--config",
                account.config.to_str().unwrap(),
                "service",
                "run",
                "--no-progress-bar",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let started = std::time::Instant::now();
    let mut first_wave = false;
    let mut classified_lookups = None;
    while photos.checks.load(Ordering::SeqCst) < 3 && started.elapsed() < Duration::from_secs(210) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "service exited: {}",
            std::fs::read_to_string(&log).unwrap()
        );
        if !first_wave
            && photos.checks.load(Ordering::SeqCst) == 1
            && std::fs::read_to_string(&log)
                .unwrap()
                .contains("Waiting before next cycle")
        {
            assert_eq!(
                count("SELECT count(*) FROM provider_active_decisions"),
                67,
                "only the exact queried identities acquire native decisions; generations={:?}; rank_pages={:?}; service={}",
                rows(&account.path, "SELECT * FROM provider_active_generations"),
                rows(
                    &account.path,
                    "SELECT generation,pass_key FROM provider_selection_rank_pages"
                ),
                std::fs::read_to_string(&log).unwrap()
            );
            first_wave = true;
        }
        if classified_lookups.is_none()
            && photos.checks.load(Ordering::SeqCst) >= 2
            && count("SELECT count(*) FROM provider_active_decisions") == 67
        {
            classified_lookups = Some(photos.lookups.load(Ordering::SeqCst));
        }
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let diagnostics = std::fs::read_to_string(&log).unwrap();
    assert!(photos.checks.load(Ordering::SeqCst) >= 3, "{diagnostics}");
    assert!(first_wave, "{diagnostics}");
    let health: Value =
        serde_json::from_slice(&std::fs::read(root.join("health.json")).unwrap()).unwrap();
    assert_eq!(
        health["consecutive_failures"], 0,
        "pending selected work is not a service failure: {health}"
    );
    assert!(health["last_error"].is_null());
    assert!(
        started.elapsed() >= Duration::from_secs(119),
        "production watch cadence must not hot-loop"
    );
    assert_eq!(
        classified_lookups,
        Some(photos.lookups.load(Ordering::SeqCst)),
        "no due work means no repeated provider lookup"
    );
    assert_eq!(
        count("SELECT count(*) FROM provider_active_decisions"),
        67,
        "quiet provider retains exact current decisions: {diagnostics}"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM assets WHERE library='PrimarySync' AND id LIKE 'asset-quiet-%'"
        ),
        0,
        "{diagnostics}"
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM assets WHERE library='OldUnknownScope' AND id='retry' AND status='pending'"
        ),
        1
    );
    assert_eq!(
        std::fs::read(&account.media).unwrap(),
        b"known media belonging only to account A"
    );
    assert_eq!(
        rows(
            &account.path,
            "SELECT * FROM provider_shadow_pages ORDER BY id LIMIT 1"
        ),
        captured
    );
    assert_eq!(
        rows(
            &account.path,
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'"
        ),
        vec![vec![rusqlite::types::Value::Text("A-cursor".into())]]
    );
    assert!(count("SELECT count(*) FROM provider_catalog_debt") >= 3);
    assert_eq!(
        count(
            "SELECT count(*) FROM provider_active_decisions WHERE admission='excluded' AND reason='currently_filtered'"
        ),
        66
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM provider_active_decisions WHERE admission='deferred' AND child='asset-quiet-66' AND reason='current_lookup_unresolved'"
        ),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM provider_work_obligations"),
        0,
        "filtered evidence is not materialized work"
    );
    assert_eq!(
        count("SELECT count(*) FROM provider_work_receipts"),
        0,
        "historical catalog does not schedule current work"
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| {
                request.url.path().ends_with("/records/lookup")
                    && serde_json::from_slice::<Value>(&request.body).unwrap()["records"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|record| record["recordName"] == "asset-quiet-66")
            }),
        "the unresolved selected identity must reach the real provider fault"
    );
    // Plain sync honors a configured watch interval; remove only scheduling
    // before the independent one-cycle restarts. Admission configuration stays
    // identical, including the date filter and source/config retry key.
    std::fs::write(&account.config, config.replace("interval=60\n", "")).unwrap();
    let frozen = state(&account.path);
    let work = rows(
        &account.path,
        "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child",
    );
    let debt = rows(
        &account.path,
        "SELECT * FROM provider_catalog_debt ORDER BY page_id,ordinal,reason",
    );
    let media = quiet_media_snapshot(account.media.parent().unwrap());
    let calls = photos.lookups.load(Ordering::SeqCst);
    for _ in 0..2 {
        let unfinished = command(root, &account, &["sync", "--no-progress-bar"]);
        assert!(
            !unfinished.status.success(),
            "selected retry debt must fail one-shot even before its retry deadline"
        );
        assert_eq!(photos.lookups.load(Ordering::SeqCst), calls);
        assert_eq!(state(&account.path), frozen);
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child"
            ),
            work
        );
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM provider_catalog_debt ORDER BY page_id,ordinal,reason"
            ),
            debt
        );
        assert_eq!(quiet_media_snapshot(account.media.parent().unwrap()), media);
    }
    let retry:(i64,i64)=Connection::open(&account.path).unwrap().query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-quiet-66' AND admission='deferred'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(retry.0, 1);
    assert!(retry.1 > chrono::Utc::now().timestamp());
    // Advance only the clock on a real provider-failed native decision. Its
    // admission hash covers the deadline; retain every source and manifest.
    let make_retry_due = || {
        let mut conn = Connection::open(&account.path).unwrap();
        let tx = conn.transaction().unwrap();
        let (generation,key,child,manifest,admission,reason,attempts):(String,String,String,String,String,String,i64) = tx.query_row(
            "SELECT generation,pass_key,child,manifest_hash,admission,reason,attempts FROM provider_active_decisions WHERE child='asset-quiet-66' AND admission='deferred'", [],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)),
        ).unwrap();
        let hash = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    &generation,
                    &key,
                    &child,
                    &manifest,
                    &admission,
                    &reason,
                    attempts,
                    1_i64
                ))
                .unwrap()
            )
        );
        tx.execute("UPDATE provider_active_decisions SET next_retry_at=1,admission_hash=?4 WHERE generation=?1 AND pass_key=?2 AND child=?3", params![generation,key,child,hash]).unwrap();
        tx.commit().unwrap();
    };
    make_retry_due();
    let retry_started = chrono::Utc::now().timestamp();
    let held = command(root, &account, &["sync", "--no-progress-bar"]);
    let held_diagnostics = String::from_utf8_lossy(&held.stderr);
    let held_health: Value =
        serde_json::from_slice(&std::fs::read(root.join("health.json")).unwrap()).unwrap();
    assert_eq!(
        held_health["consecutive_failures"], 0,
        "provider retry debt stays healthy: {held_health}"
    );
    assert!(
        !held.status.success(),
        "the due current confirmation still fails: {held_diagnostics}"
    );
    assert!(
        held_diagnostics.contains("UNKNOWN_ITEM"),
        "{held_diagnostics}"
    );
    assert!(
        held_diagnostics.contains("Selected identity retry is not due"),
        "the immediate tail retry must honor the newly durable deadline: {held_diagnostics}"
    );
    let retry_finished = chrono::Utc::now().timestamp();
    assert_eq!(photos.lookups.load(Ordering::SeqCst), calls + 1);
    let retry:(i64,i64)=Connection::open(&account.path).unwrap().query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE child='asset-quiet-66' AND admission='deferred'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(retry.0, 2);
    assert!((retry_started + 7200..=retry_finished + 7200).contains(&retry.1));
    assert_eq!(
        count(
            "SELECT count(*) FROM provider_active_decisions WHERE child='asset-quiet-66' AND admission='deferred' AND reason='current_lookup_unresolved'"
        ),
        1
    );
    assert_eq!(state(&account.path), frozen);
    assert_eq!(quiet_media_snapshot(account.media.parent().unwrap()), media);
    assert_eq!(
        rows(
            &account.path,
            "SELECT * FROM provider_catalog_debt ORDER BY page_id,ordinal,reason"
        ),
        debt
    );
    // Recover the same selected identity through real paired confirmation.
    // Changing provider availability alone cannot bypass its durable deadline.
    photos.tail_available.store(true, Ordering::SeqCst);
    let retained_retry = rows(
        &account.path,
        "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child",
    );
    assert!(
        !command(root, &account, &["sync", "--no-progress-bar"])
            .status
            .success()
    );
    assert_eq!(photos.lookups.load(Ordering::SeqCst), calls + 1);
    assert_eq!(
        rows(
            &account.path,
            "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child"
        ),
        retained_retry
    );
    make_retry_due();
    let recovery_requests = server.received_requests().await.unwrap().len();
    success(&command(root, &account, &["sync", "--no-progress-bar"]));
    assert_eq!(
        photos.lookups.load(Ordering::SeqCst),
        calls + 2,
        "the refreshed rank observation supplies both keys for one paired confirmation"
    );
    let requests = server.received_requests().await.unwrap();
    let lookups: Vec<Value> = requests[recovery_requests..]
        .iter()
        .filter(|request| request.url.path().ends_with("/records/lookup"))
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(lookups.len(), 1, "only the due identity is reconfirmed");
    let lookup = &lookups[0];
    assert_eq!(
        lookup["zoneID"],
        json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})
    );
    let actual: Vec<_> = lookup["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["recordName"].as_str().unwrap())
        .collect();
    assert_eq!(
        actual,
        ["asset-quiet-66", "quiet-66"],
        "the exact child and master must be confirmed together"
    );
    assert_eq!(
        count("SELECT count(*) FROM provider_active_decisions WHERE admission='deferred'"),
        0
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM provider_active_decisions WHERE admission='excluded' AND reason='currently_filtered'"
        ),
        67
    );
    let completed = rows(
        &account.path,
        "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child",
    );
    for _ in 0..2 {
        success(&command(root, &account, &["sync", "--no-progress-bar"]));
        assert_eq!(photos.lookups.load(Ordering::SeqCst), calls + 2);
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child"
            ),
            completed
        );
        assert_eq!(state(&account.path), frozen);
        assert_eq!(quiet_media_snapshot(account.media.parent().unwrap()), media);
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM provider_catalog_debt ORDER BY page_id,ordinal,reason"
            ),
            debt
        );
        assert_eq!(
            rows(
                &account.path,
                "SELECT * FROM provider_shadow_pages ORDER BY id LIMIT 1"
            ),
            captured
        );
    }
}

struct QuietServiceChild(std::process::Child);
impl Drop for QuietServiceChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn quiet_media_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut media = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes, modified)
        })
        .collect::<Vec<_>>();
    media.sort_by(|a, b| a.0.cmp(&b.0));
    media
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inventory_upgrade_actual_cli_expands_fixed_dates_and_preserves_other_account() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let server = MockServer::start().await;
    let photos = RecentCliPhotos {
        first_delta: Arc::new(AtomicBool::new(true)),
        queries: Arc::new(AtomicUsize::new(0)),
    };
    Mock::given(method("POST"))
        .respond_with(photos.clone())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(QUIET_MEDIA))
        .expect(0)
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    let other = seed(root, ACCOUNTS[1], "B", &server.uri());
    // The actual binary enforces HTTPS CDN URLs. Exercise its existing local
    // name-id7 existing-file skip owner, never the synthetic URL.
    let text = std::fs::read_to_string(&account.config).unwrap();
    std::fs::write(
        &account.config,
        text.replace(
            "[ui]",
            "folder_structure=\"\"\n[photos]\nfile_match_policy=\"name-id7\"\n[ui]",
        ),
    )
    .unwrap();
    for number in 0..3 {
        std::fs::write(
            account
                .media
                .parent()
                .unwrap()
                .join(format!("recent-{number}_YXNzZXQ.JPG")),
            QUIET_MEDIA,
        )
        .unwrap();
    }
    Connection::open(&account.path).unwrap().execute_batch(
        "UPDATE assets SET library='OldUnknownScope' WHERE id='retry';
         UPDATE metadata SET key='sync_token:OldUnknownScope' WHERE key='pending_sync_token:old:PrimarySync';
         INSERT INTO metadata(key,value) VALUES('recent_selection_recovery:PrimarySync','unparsed old receipt');
         PRAGMA user_version=36;"
    ).unwrap();
    let other_before = state(&other.path);
    let old_media = quiet_media_snapshot(account.media.parent().unwrap());
    for cycle in 0..5 {
        let arguments = match cycle {
            0 => vec![
                "sync",
                "--skip-created-before",
                "2021-01-01",
                "--no-progress-bar",
            ],
            1 => vec![
                "sync",
                "--skip-created-before",
                "2020-01-01",
                "--no-progress-bar",
            ],
            _ => vec!["sync", "--no-progress-bar"],
        };
        let before_queries = photos.queries.load(Ordering::SeqCst);
        let output = clean_cmd()
            .env("KEI_DATA_DIR", root)
            .env("ICLOUD_USERNAME", account.username)
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args(["--config", account.config.to_str().unwrap()])
            .args(&arguments)
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap();
        success(&output);
        let conn = Connection::open(&account.path).unwrap();
        // name-id7's existing-file policy skips without inventing downloaded
        // state. Trace the actual selected byte paths; fresh publication and
        // finalization are qualified by the dispatcher companion/uncapped tests.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let names: Vec<_> = (0..3)
            .map(|n| format!("recent-{n}_YXNzZXQ.JPG"))
            .filter(|name| stderr.contains(name))
            .collect();
        if cycle == 0 {
            assert_eq!(names, ["recent-1_YXNzZXQ.JPG", "recent-2_YXNzZXQ.JPG"]);
            let cursor: String = conn
                .query_row(
                    "SELECT value FROM historical_v030_checkpoints WHERE key='sync_token:PrimarySync'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(cursor, "A-cursor");
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                0
            );
            let raw: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='recent_selection_recovery:PrimarySync'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(raw, "unparsed old receipt");
            assert_eq!(conn.query_row::<i64,_,_>("SELECT COUNT(*) FROM metadata WHERE key='inventory_reconciliation_required:PrimarySync'",[],|row|row.get(0)).unwrap(),0);
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_catalog_records WHERE record_type='CPLAsset'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0,
                "fresh-start rank enumeration must not replay the retired delta cursor"
            );
        } else {
            if cycle <= 2 {
                assert_eq!(
                    names,
                    [
                        "recent-0_YXNzZXQ.JPG",
                        "recent-1_YXNzZXQ.JPG",
                        "recent-2_YXNzZXQ.JPG"
                    ]
                );
            } else {
                assert!(names.is_empty());
            }
            let cursor: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                cursor,
                if cycle == 1 {
                    "recent-rank-token"
                } else {
                    "recent-delta-successor"
                },
                "the first inventory establishes a new boundary; later changes bridge from it"
            );
            if cycle >= 3 {
                assert_eq!(
                    photos.queries.load(Ordering::SeqCst),
                    before_queries,
                    "quiet unchanged reopened CLI must not repeat inventory"
                );
            }
        }
        assert_eq!(conn.query_row::<String,_,_>("SELECT last_error FROM assets WHERE library='OldUnknownScope' AND id='retry' AND status='pending'",[],|r|r.get(0)).unwrap(), "A-retry-error");
        assert_eq!(state(&other.path), other_before);
        for (path, bytes, modified) in &old_media {
            assert_eq!(std::fs::read(path).unwrap(), *bytes);
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                *modified
            );
        }
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT value FROM historical_v030_checkpoints WHERE key='sync_token:OldUnknownScope'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "A-historical-debt"
        );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_selection_actual_cli_and_watch_reopen_independent_destination_debt() {
    private_cli_destination_recovery(false).await;
}

#[cfg(feature = "xmp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_cli_commits_provider_token_before_local_metadata_failure_and_restarts() {
    private_cli_destination_recovery(true).await;
}

async fn private_cli_destination_recovery(metadata_fault: bool) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = MockServer::start().await;
    let photos = PrivateSelectionCliPhotos {
        ranks: Arc::new(AtomicUsize::new(0)),
        rank_cycles: Arc::new(std::sync::Mutex::new(Vec::new())),
        lookups: Arc::new(AtomicUsize::new(0)),
        checks: Arc::new(AtomicUsize::new(0)),
    };
    Mock::given(method("POST"))
        .respond_with(photos.clone())
        .mount(&server)
        .await;
    // Production URL validation remains in force. Real previously verified
    // synthetic local bytes qualify receipt replay; fresh transfers are proved
    // separately through the dispatcher publication histories.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(QUIET_MEDIA))
        .expect(0)
        .mount(&server)
        .await;
    // Integrity inspection refuses symlink ancestors, including macOS /var.
    #[cfg(target_os = "macos")]
    let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    #[cfg(not(target_os = "macos"))]
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let account = seed(root, ACCOUNTS[0], "A", &server.uri());
    let media_root = account.media.parent().unwrap();
    let old_bytes = std::fs::read(&account.media).unwrap();
    let checksum = format!("{:x}", Sha256::digest(QUIET_MEDIA));
    let provider_checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(QUIET_MEDIA));
    let mut seeded = Vec::new();
    let conn = Connection::open(&account.path).unwrap();
    conn.execute_batch("UPDATE assets SET library='OldUnknownScope' WHERE id='retry'; DELETE FROM metadata WHERE key='pending_sync_token:old:PrimarySync';").unwrap();
    drop(conn);
    // Migrate the schema-33 synthetic seed before binding the album roots.
    Connection::open(&account.path).unwrap().execute_batch("DROP TABLE provider_active_sources; DROP TABLE provider_active_destinations; DROP TABLE provider_active_decisions; DROP TABLE provider_selection_rank_records; DROP TABLE provider_selection_rank_pages; DROP TABLE provider_active_generations; PRAGMA user_version=33;").unwrap();
    success(&command(root, &account, &["status"]));
    let config=std::fs::read_to_string(&account.config).unwrap().replace("[ui]","folder_structure=\"\"\nfolder_structure_albums=\"{album}\"\n[filters]\nalbums=[\"A\",\"B\"]\nunfiled=true\n[ui]");
    let config = if metadata_fault {
        format!("{config}[metadata]\nxmp_sidecar=true\n")
    } else {
        config
    };
    // Start with a selection that binds the two directories but selects no
    // media. Precreating A and B would deliberately reserve external trees.
    std::fs::write(
        &account.config,
        config.replace(
            "unfiled=true",
            "unfiled=false\nskip_created_after=\"2000-01-01\"",
        ),
    )
    .unwrap();
    success(&command(root, &account, &["sync", "--no-progress-bar"]));
    let conn = Connection::open(&account.path).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM album_containers WHERE directory_binding IS NOT NULL",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        2
    );
    for (album, number) in [("A", 0), ("A", 1), ("B", 0), ("B", 2), ("", 3)] {
        let folder = media_root.join(album);
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("private-{number}.JPG"));
        std::fs::write(&path, QUIET_MEDIA).unwrap();
        let id = format!("asset-private-{number}");
        if album != "B" || number != 0 {
            insert_asset(
                &conn,
                &id,
                "downloaded",
                &format!("private-{number}.JPG"),
                Some(path.to_str().unwrap()),
                None,
                Some(&checksum),
            );
            conn.execute(
                "UPDATE assets SET size_bytes=?1,checksum=?2 WHERE id=?3",
                params![QUIET_MEDIA.len() as i64, provider_checksum, id],
            )
            .unwrap();
            conn.execute("INSERT INTO asset_metadata_capture_revisions(library,asset_id,revision,updated_at) VALUES('PrimarySync',?1,1,1700000000)",[&id]).unwrap();
        }
        conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum) VALUES('PrimarySync',?1,'original',?2,?3,?4,?4,?4)",params![id,path.to_str().unwrap(),provider_checksum,checksum]).unwrap();
        seeded.push((
            path.clone(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        ));
    }
    drop(conn);
    let conn = Connection::open(&account.path).unwrap();
    if metadata_fault {
        conn.execute_batch("CREATE TRIGGER backfill_commit_fault BEFORE UPDATE OF verified_metadata ON provider_active_destinations WHEN OLD.verified_metadata=0 AND NEW.verified_metadata=1 BEGIN SELECT RAISE(ABORT,'synthetic backfill receipt failure'); END;").unwrap();
    } else {
        conn.execute_batch("CREATE TRIGGER private_cli_destination_fault BEFORE UPDATE OF verified_media ON provider_active_destinations WHEN NEW.verified_media=1 AND replace(NEW.compat_path,char(92),'/') LIKE '%/B/%' BEGIN SELECT RAISE(ABORT,'synthetic private B receipt interruption'); END;").unwrap();
    }
    // Start the faulted cycle without a provider cursor, as on first sync.
    conn.execute(
        "DELETE FROM metadata WHERE key='sync_token:PrimarySync'",
        [],
    )
    .unwrap();
    drop(conn);
    std::fs::write(&account.config, &config).unwrap();
    if metadata_fault {
        let conn = Connection::open(&account.path).unwrap();
        conn.execute_batch("CREATE TRIGGER failed_writer_intent BEFORE UPDATE OF intent_hash ON provider_active_destinations WHEN NEW.intent_hash IS NOT NULL BEGIN SELECT RAISE(ABORT,'synthetic failure to save metadata intent'); END;").unwrap();
        let failed = command(root, &account, &["sync", "--no-progress-bar"]);
        assert!(!failed.status.success());
        assert!(
            String::from_utf8_lossy(&failed.stderr)
                .contains("synthetic failure to save metadata intent")
        );
        assert!(
            !conn
                .query_row::<bool, _, _>(
                    "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='sync_token:PrimarySync')",
                    [],
                    |r| r.get(0)
                )
                .unwrap()
        );
        conn.execute_batch("DROP TRIGGER failed_writer_intent")
            .unwrap();
    }
    let out = command(root, &account, &["sync", "--no-progress-bar"]);
    let diagnostics = String::from_utf8_lossy(&out.stderr);
    if metadata_fault {
        assert!(
            !out.status.success() && diagnostics.contains("synthetic backfill receipt failure"),
            "{diagnostics}"
        );
        let conn = Connection::open(&account.path).unwrap();
        assert!(conn.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM metadata WHERE key='sync_token:PrimarySync' AND length(value)>0)",[],|r|r.get(0)).unwrap(),"{diagnostics}");
        let remaining = || {
            conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1 AND verified_metadata=0",[],|r|r.get(0)).unwrap()
        };
        assert_eq!(remaining(), 5);
        let ranks = photos.ranks.load(Ordering::SeqCst);
        conn.execute_batch("DROP TRIGGER backfill_commit_fault")
            .unwrap();
        success(&command(root, &account, &["sync", "--no-progress-bar"]));
        assert_eq!(remaining(), 0);
        assert_eq!(
            photos.ranks.load(Ordering::SeqCst),
            ranks,
            "a durable provider token must avoid a repeat inventory"
        );
        for (path, modified) in &seeded {
            assert_eq!(std::fs::read(path).unwrap(), QUIET_MEDIA);
            assert_eq!(
                std::fs::metadata(path).unwrap().modified().unwrap(),
                *modified
            );
            assert!(path.with_extension("JPG.xmp").is_file());
        }
        return;
    }
    assert!(
        diagnostics.contains("synthetic private B receipt interruption"),
        "{diagnostics}"
    );
    assert!(
        !out.status.success(),
        "unfinished B must fail the one-shot: {diagnostics}"
    );
    let count = |sql: &str| {
        Connection::open(&account.path)
            .unwrap()
            .query_row::<i64, _, _>(sql, [], |r| r.get(0))
            .unwrap()
    };
    assert!(
        count("SELECT count(*) FROM provider_active_generations") > 0,
        "actual startup must activate named private passes"
    );
    assert!(
        count(
            "SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND replace(compat_path,char(92),'/') LIKE '%/A/%'"
        ) > 0
    );
    assert!(
        count(
            "SELECT count(*) FROM provider_active_destinations WHERE verified_media=0 AND replace(compat_path,char(92),'/') LIKE '%/B/%'"
        ) > 0
    );
    assert_eq!(
        count("SELECT count(*) FROM assets WHERE id='asset-private-0' AND status='downloaded'"),
        1,
        "canonical A completion cannot hide independent B debt"
    );
    assert_eq!(
        rows(
            &account.path,
            "SELECT value FROM historical_v030_checkpoints WHERE key='sync_token:PrimarySync'"
        ),
        vec![vec![rusqlite::types::Value::Text("A-cursor".into())]]
    );
    assert!(
        rows(
            &account.path,
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'"
        )
        .is_empty()
    );
    Connection::open(&account.path)
        .unwrap()
        .execute_batch("DROP TRIGGER private_cli_destination_fault;")
        .unwrap();
    std::fs::write(
        &account.config,
        format!("{config}[watch]\ninterval=60\n[server]\nport=0\n"),
    )
    .unwrap();
    let health_path = root.join("health.json");
    let health_stamp = || {
        // Health publication is a whole-cycle boundary. A concurrent plain
        // write can be briefly incomplete; retry rather than count that as one.
        std::fs::read(&health_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.get("last_sync_at")?.as_str().map(str::to_owned))
    };
    let mut last_health_stamp = health_stamp();
    assert!(
        last_health_stamp.is_some(),
        "initial CLI cycle health receipt"
    );
    let log = root.join("private-selection-service.log");
    let mut child = QuietServiceChild(
        std::process::Command::new(assert_cmd::cargo::cargo_bin!("kei"))
            .env_clear()
            .envs(std::env::var_os("SystemRoot").map(|v| ("SystemRoot", v)))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root)
            .env("KEI_DATA_DIR", root)
            .env("ICLOUD_USERNAME", account.username)
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args([
                "--config",
                account.config.to_str().unwrap(),
                "service",
                "run",
                "--no-progress-bar",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let started = std::time::Instant::now();
    let mut recovered = None;
    let mut completed_cycles = 0;
    while completed_cycles < 3 && started.elapsed() < Duration::from_secs(210) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&log).unwrap()
        );
        let Some(stamp) = health_stamp() else {
            continue;
        };
        if last_health_stamp.as_ref() == Some(&stamp) {
            continue;
        }
        last_health_stamp = Some(stamp);
        completed_cycles += 1;
        let counters = (
            photos.ranks.load(Ordering::SeqCst),
            photos.lookups.load(Ordering::SeqCst),
        );
        if completed_cycles == 1 {
            assert_eq!(
                count(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=1"
                ),
                5,
                "recovery must finish all media and metadata before quiet sampling"
            );
            assert_eq!(
                count(
                    "SELECT count(*) FROM provider_active_destinations WHERE verified_media=0 OR verified_metadata=0"
                ),
                0
            );
            assert_eq!(
                count("SELECT count(*) FROM provider_active_generations WHERE sealed=0"),
                0
            );
            recovered = Some(counters);
        } else {
            assert_eq!(
                recovered,
                Some(counters),
                "completed quiet cycle {completed_cycles}"
            );
        }
    }
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let diagnostics = std::fs::read_to_string(&log).unwrap();
    assert_eq!(completed_cycles, 3, "{diagnostics}");
    assert!(photos.checks.load(Ordering::SeqCst) >= 3, "{diagnostics}");
    assert!(
        photos
            .rank_cycles
            .lock()
            .unwrap()
            .iter()
            .all(|(phase, _)| *phase <= 1),
        "quiet watch phases must issue no rank requests: {:?}",
        photos.rank_cycles.lock().unwrap().clone()
    );
    assert!(
        started.elapsed() >= Duration::from_secs(119),
        "real watch cadence"
    );
    assert_eq!(
        recovered,
        Some((
            photos.ranks.load(Ordering::SeqCst),
            photos.lookups.load(Ordering::SeqCst)
        )),
        "two unchanged watch cycles must not repeat completed selection/hydration; rank phases={:?}",
        photos.rank_cycles.lock().unwrap().clone()
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM provider_active_destinations WHERE verified_media=1 AND verified_metadata=1"
        ),
        5,
        "all pass destinations, not canonical asset count"
    );
    for (path, modified) in seeded {
        assert_eq!(std::fs::read(&path).unwrap(), QUIET_MEDIA);
        assert_eq!(
            std::fs::metadata(path).unwrap().modified().unwrap(),
            modified
        );
    }
    assert_eq!(std::fs::read(&account.media).unwrap(), old_bytes);
    let target = media_root.join("B").join("private-0.JPG");
    let peer = media_root.join("A").join("private-0.JPG");
    // The service portion used watch mode. Repair commands below are one-shot.
    std::fs::write(&account.config, &config).unwrap();
    std::fs::write(&target, b"confirmed damaged bytes").unwrap();
    let inspected = command(
        root,
        &account,
        &["reconcile", "--inspect-path", target.to_str().unwrap()],
    );
    success(&inspected);
    let text = String::from_utf8(inspected.stdout).unwrap();
    let token = text
        .lines()
        .find_map(|line| line.strip_prefix("Fingerprint: "))
        .unwrap();
    success(&command(
        root,
        &account,
        &[
            "reconcile",
            "--inspect-path",
            target.to_str().unwrap(),
            "--confirm-damage",
            token,
        ],
    ));
    let retried = command(root, &account, &["sync", "--no-progress-bar"]);
    assert!(
        !retried.status.success(),
        "unavailable source must retain incomplete repair"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"confirmed damaged bytes");
    assert_eq!(std::fs::read(&peer).unwrap(), QUIET_MEDIA);
    let conn = Connection::open(&account.path).unwrap();
    let pending: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE confirmed_repair IS NOT NULL AND verified_media=0 AND source_checksum IS NULL", [], |r| r.get(0)).unwrap();
    assert!(
        pending > 0,
        "confirmation must survive actual CLI restart without cloud provenance"
    );
    if let (Ok(proxy), Ok(ca)) = (
        std::env::var("KEI_INTEGRITY_TLS_PROXY"),
        std::env::var("KEI_INTEGRITY_TLS_CA"),
    ) {
        conn.execute("UPDATE provider_active_decisions SET next_retry_at=0", [])
            .unwrap();
        drop(conn);
        let repaired = clean_cmd()
            .env("KEI_DATA_DIR", root)
            .env("ICLOUD_USERNAME", account.username)
            .env("HTTPS_PROXY", proxy)
            .env("SSL_CERT_FILE", ca)
            .args([
                "--config",
                account.config.to_str().unwrap(),
                "sync",
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(30))
            .output()
            .unwrap();
        success(&repaired);
        assert_eq!(std::fs::read(&target).unwrap(), QUIET_MEDIA);
        assert_eq!(std::fs::read(&peer).unwrap(), QUIET_MEDIA);
        success(&command(root, &account, &["sync", "--no-progress-bar"]));
        let conn = Connection::open(&account.path).unwrap();
        assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_active_destinations WHERE confirmed_repair IS NOT NULL", [], |row| row.get(0)).unwrap(), 0);
    }
}
