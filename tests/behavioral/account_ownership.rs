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
    std::fs::write(&config,format!("[auth]\nusername={}\npassword_file={}\n[download]\ndirectory={}\n[ui]\nfriendly=false\nprogress_bar=false\n",
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
            json!({"zones":[]})
        } else if body
            .get("query")
            .and_then(|query| query.get("recordType"))
            .and_then(Value::as_str)
            == Some("CheckIndexingState")
        {
            json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]})
        } else if path.ends_with("/changes/zone") {
            json!({"zones":[{"zoneID":{"zoneName":"PrimarySync"},"syncToken":format!("{label}-cursor"),"moreComing":false,"records":[]}]})
        } else if path.ends_with("/records/lookup") {
            json!({"records":[{"recordName":"retry","serverErrorCode":"UNKNOWN_ITEM"}]})
        } else if path.ends_with("/internal/records/query/batch") {
            json!({"batch":[{"records":[{"fields":{"itemCount":{"value":1}}}]}]})
        } else if path.ends_with("/records/query") {
            json!({"records":[]})
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
            let other_state = state(&other.path);
            let other_bytes = std::fs::read(&other.path).unwrap();
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
                state(&other.path),
                other_state,
                "{args:?}: other account rows"
            );
            assert_eq!(
                std::fs::read(&other.path).unwrap(),
                other_bytes,
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

fn seed_restore(root: &Path, account: &Account, server: &str) -> Account {
    // Fresh seed reuses existing synthetic media/config directories only.
    std::fs::remove_dir_all(account.media.parent().unwrap()).unwrap();
    seed(root, account.username, account.label, server)
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
            let quiet = body.pointer("/zones/0/syncToken").and_then(Value::as_str)
                == Some("delta-successor");
            let mut response = json!({"zones":[{
                "zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                "syncToken":"delta-successor","moreComing":false,
                "records":if quiet { json!([]) } else { json!([{
                    "recordName":"future-source","recordType":"FutureRecord",
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
                conn.execute_batch("CREATE TRIGGER shadow_cli_fault BEFORE INSERT ON provider_shadow_receipts BEGIN SELECT RAISE(ABORT,'synthetic CLI receipt failure'); END;").unwrap();
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
        for _ in 0..2 {
            let out = command(root, &account, &["sync", "--no-progress-bar"]);
            let diagnostics = String::from_utf8_lossy(&out.stderr);
            assert!(diagnostics.contains(expected), "{kind}: {diagnostics}");
            assert!(
                !diagnostics.contains("falling back to full enumeration"),
                "{kind}: {diagnostics}"
            );
            assert!(!diagnostics.contains("private-other"));
            assert_eq!(
                state(&account.path),
                before,
                "{kind}: retained owned history/checkpoint"
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
        success(&command(root, &account, &["sync", "--no-progress-bar"]));
        for _ in 0..2 {
            // Actual CLI processes reopen independently; the second empty
            // page replay must not grow observations or repeat publication.
            success(&command(root, &account, &["sync", "--no-progress-bar"]));
            let conn = Connection::open(&account.path).unwrap();
            assert_eq!(
                conn.query_row::<String, _, _>(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                "delta-successor",
                "{kind}"
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                2,
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
        for request in server.received_requests().await.unwrap() {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(
                !(request.url.path().ends_with("/records/query")
                    && body["query"]["recordType"]
                        .as_str()
                        .is_some_and(|kind| kind.starts_with("CPLAsset"))),
                "{kind}: rank fallback must never run"
            );
        }
    }
}
