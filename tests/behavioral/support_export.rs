//! Real offline command boundary: no auth, credential commands, state migration or media scans.
use super::support::{clean_cmd, sanitize_username};
use serde_json::Value;
use std::path::Path;

#[test]
fn docker_export_query_parses_only_arguments_without_collecting() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("private-config.toml");
    let output = root.path().join("bundle.json");
    let marker = root.path().join("credential-command-ran");
    std::fs::write(
        &config,
        format!(
            "[auth]\nusername = \"private-user@example.invalid\"\npassword_command = {}\n",
            crate::common::toml_string(&format!("touch {}", marker.display()))
        ),
    )
    .unwrap();
    let before = snapshot(root.path());
    for (arguments, expected) in [
        (
            vec![
                "--config",
                config.to_str().unwrap(),
                "support-export",
                "--output",
                output.to_str().unwrap(),
            ],
            0,
        ),
        (vec!["support-export"], 0),
        (vec!["support-export", "--output"], 0),
        (vec!["support-export", "--help"], 0),
        (vec!["help", "support-export"], 0),
        (vec!["--config", "support-export", "help", "status"], 1),
        (vec!["--config=support-export", "status"], 1),
        (vec!["--config", "support-export", "status"], 1),
        (vec!["sync", "--password-command", "support-export"], 1),
        (vec!["sync"], 1),
        (vec!["--version"], 1),
        (vec![], 1),
    ] {
        let result = clean_cmd()
            .env("KEI_DOCKER_ENTRYPOINT_QUERY", "1")
            .env("KEI_DATA_DIR", root.path().join("absent-data"))
            .env("ICLOUD_PASSWORD", "private-secret")
            .env("KEI_PASSWORD_FILE", "private-secret-file")
            .env("KEI_PASSWORD_COMMAND", "private-secret-command")
            .env("KEI_LOG_LEVEL", "private-invalid-level")
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(expected),
            "{arguments:?}: {result:?}"
        );
        assert!(
            result.stdout.is_empty() && result.stderr.is_empty(),
            "{arguments:?}: {result:?}"
        );
        assert_eq!(snapshot(root.path()), before);
        assert!(!marker.exists() && !output.exists());
        assert!(!root.path().join("absent-data").exists());
    }
}

fn snapshot(directory: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            ));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

#[test]
fn support_export_is_readonly_with_old_schema_and_malicious_inputs() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("private-data");
    std::fs::create_dir(&data).unwrap();
    let media = root.path().join("private-media");
    std::fs::create_dir(&media).unwrap();
    std::fs::write(media.join("private-id.jpg"), b"private-media-content").unwrap();
    let config = root.path().join("config.toml");
    let output = root.path().join("bundle.json");
    let marker = root.path().join("password-command-ran");
    std::fs::write(&config, format!("[auth]\nusername = \"private-user@example.invalid\"\npassword_command = {}\n[download]\ndirectory = {}\n[filters]\nalbums = [\"private-album\"]\n",
        crate::common::toml_string(&format!("touch {}", marker.display())), crate::common::toml_string(&media.display().to_string()))).unwrap();
    let db_path = data.join(format!(
        "{}.db",
        sanitize_username("private-user@example.invalid")
    ));
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute_batch(
        "CREATE TABLE private_data(secret TEXT); INSERT INTO private_data VALUES('private-token');",
    )
    .unwrap();
    drop(conn);
    std::fs::write(data.join("health.json"), br#"{"last_error":"https://private-url password=private-secret","consecutive_failures":4,"last_sync_at":"2026-10-10T16:00:00Z"}"#).unwrap();
    let before = snapshot(&data);
    let media_before = snapshot(&media);
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .env("RUST_LOG", "trace")
        .env("ICLOUD_PASSWORD", "private-password")
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    assert!(!marker.exists());
    assert_eq!(snapshot(&data), before);
    assert_eq!(snapshot(&media), media_before);
    let bytes = std::fs::read(&output).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("private-"), "{text}");
    let bundle: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(bundle["schema_version"], 1);
    assert_eq!(bundle["state"]["status"], "older_schema");
    assert_eq!(bundle["state"]["migration_performed"], false);
    assert_eq!(bundle["health"]["counters"]["consecutive_failures"], 4);
    // Existing output is never replaced, including a state/media path.
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&db_path)
        .assert()
        .failure();
    assert_eq!(snapshot(&data), before);
}

#[test]
fn support_export_missing_invalid_state_and_config_still_produces_actionable_file() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("absent-data");
    let config = root.path().join("broken.toml");
    let output = root.path().join("bundle.json");
    std::fs::write(
        &config,
        "password = \"private-secret\"\nunknown_field = \"https://private-url\"\n",
    )
    .unwrap();
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    assert!(!data.exists());
    let text = std::fs::read_to_string(output).unwrap();
    assert!(!text.contains("private-secret"));
    let bundle: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(bundle["configuration"]["status"], "invalid_or_unreadable");
    assert_eq!(bundle["summary"]["partial"], true);
    assert!(!bundle["next_actions"].as_array().unwrap().is_empty());
}

#[test]
fn support_export_never_opens_live_wal_or_creates_sqlite_companions() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let username = "private-user@example.invalid";
    let path = data.join(format!("{}.db", sanitize_username(username)));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE private_data(secret TEXT); INSERT INTO private_data VALUES('private-secret')",
    )
    .unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!("[auth]\nusername={}", crate::common::toml_string(username)),
    )
    .unwrap();
    let before = snapshot(&data);
    let output = root.path().join("bundle.json");
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    assert_eq!(snapshot(&data), before);
    let bundle: Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(bundle["state"]["status"], "live_wal_snapshot_unavailable");
    assert_eq!(bundle["state"]["wal_present"], true);
}

#[test]
fn support_history_survives_a_real_process_restart_without_new_sync() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let config = root.path().join("config.toml");
    let report = root.path().join("private-report.json");
    std::fs::write(&config, format!("[auth]\nusername=\"private-user@example.invalid\"\n[download]\ndirectory={}\n[report]\njson={}\n",crate::common::toml_string(&media.display().to_string()),crate::common::toml_string(&report.display().to_string()))).unwrap();
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .env("KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS", "1")
        .env("RUST_LOG", "error")
        .arg("--config")
        .arg(&config)
        .arg("sync")
        .assert()
        .success();
    let before = snapshot(&data);
    let output = root.path().join("support.json");
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    assert_eq!(snapshot(&data), before);
    let text = std::fs::read_to_string(output).unwrap();
    assert!(!text.contains("private-"));
    let bundle: Value = serde_json::from_str(&text).unwrap();
    let records = bundle["history"]["data"]["cycles"].as_array().unwrap();
    assert!(
        records
            .iter()
            .any(|c| c["stats"]["bytes_downloaded"] == 4096)
    );
    assert_eq!(bundle["history"]["status"], "available");
}

#[test]
fn support_export_current_owned_state_is_bounded_and_wrong_owner_degrades() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let username = "private-user@example.invalid";
    let conn = super::support::create_state_db(&data, username);
    conn.execute_batch("WITH RECURSIVE n(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM n WHERE value<10005) INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,last_seen_at,status) SELECT 'PRIVATE_ZONE',CAST(value AS TEXT),'original','PRIVATE_CHECKSUM','PRIVATE_FILE',1,1,'photo',1,CASE WHEN value=1 THEN 'failed' ELSE 'pending' END FROM n").unwrap();
    drop(conn);
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!("[auth]\nusername={}", crate::common::toml_string(username)),
    )
    .unwrap();
    let before = snapshot(&data);
    let output = root.path().join("support.json");
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    assert_eq!(snapshot(&data), before);
    let text = std::fs::read_to_string(output).unwrap();
    assert!(!text.contains("PRIVATE_"));
    let bundle: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(bundle["state"]["status"], "available");
    assert_eq!(bundle["state"]["complete"], false);
    assert_eq!(bundle["state"]["observed_asset_rows"], 10001);
    assert_eq!(bundle["state"]["omitted_rows"], "unknown");
    let path = data.join(format!("{}.db", sanitize_username(username)));
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "UPDATE account_owner SET account_key='private-wrong-account'",
        [],
    )
    .unwrap();
    drop(conn);
    let output = root.path().join("wrong-owner.json");
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    let bundle: Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(
        bundle["state"]["status"],
        "account_owner_unavailable_or_mismatched"
    );
    assert!(bundle["state"].get("status_counts").is_none());
}

#[test]
fn support_export_owner_provenance_roundtrip_preserves_old_unavailable_fields() {
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let username = "private-user@example.invalid";
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!("[auth]\nusername={}", crate::common::toml_string(username)),
    )
    .unwrap();
    let diagnostic = |kind: &str, fields: Value| {
        json!({
            "kind":kind,"fields":fields,"observations":1,
            "first_at":"2026-10-11T00:00:00Z","last_at":"2026-10-11T00:00:00Z",
        })
    };
    let cycle = |diagnostics: Vec<Value>| {
        json!({
            "id":uuid::Uuid::new_v4().to_string(),"started_at":"2026-10-11T00:00:00Z","completed_at":"2026-10-11T00:01:00Z",
            "outcome":"partial_failure","build_version":"0.25.1","configuration":{},"stats":{},"diagnostics":diagnostics,
        })
    };
    let new_inventory = |owner: &str, owner_type: &str| {
        diagnostic(
            "legacy_inventory_failure_v2",
            json!({
                "phase":"child_reference","reason":"invalid_inventory_evidence","subreason":"child_reference_scope_mismatch",
                "scope_mismatch_component":"owner","expected_owner":"absent","expected_owner_type":"absent",
                "supplied_owner":owner,"supplied_owner_type":owner_type,"eof_observed":true,"pages":461,
            }),
        )
    };
    let current = cycle(vec![
        new_inventory("private_default", "string"),
        new_inventory("other_string", "string"),
        new_inventory("null", "null"),
        new_inventory("malformed", "number"),
        diagnostic(
            "support_discovery_v1",
            json!({"phase":"primary_discovery","database":"private","zone_class":"primary","listing_validated":true,
            "primary_eligibility":"eligible","owner":"private_default","owner_type":"string","qualification_published":false}),
        ),
        diagnostic(
            "support_discovery_v1",
            json!({"phase":"selected_qualified_indexing","database":"private","zone_class":"other",
            "owner":"private_default","owner_type":"string","selected_indexing":true,"qualification_published":true}),
        ),
        diagnostic(
            "support_discovery_v1",
            json!({"phase":"primary_publication","database":"private","zone_class":"primary",
            "owner":"private_default","owner_type":"string","selected_indexing":true,"qualification_published":true}),
        ),
        diagnostic(
            "support_missing_reference_v1",
            json!({
            "record_history":"replacement", "prior_reference":"present", "sparse_marker":"true",
            "linkage":"partial_fields", "lookup_outcome":"live_reference", "lookup_family":"candidate",
            "lookup_scope":"matched", "lookup_shape":"live_reference",
            "raw_reference":"PRIVATE_REFERENCE_CANARY"}),
        ),
    ]);
    let old = cycle(vec![
        diagnostic(
            "legacy_inventory_failure_v2",
            json!({"phase":"child_reference","scope_mismatch_component":"owner","pages":461}),
        ),
        diagnostic(
            "support_discovery_v1",
            json!({"phase":"selected_qualified_indexing","database":"private","owner":"other","selected_indexing":true,"qualification_published":true}),
        ),
        diagnostic(
            "support_missing_reference_v1",
            json!({"record_history":"not_recorded", "lookup_outcome":"scope_unavailable", "lookup_family":"not_established"}),
        ),
    ]);
    let malicious = cycle(vec![
        diagnostic(
            "legacy_inventory_failure_v2",
            json!({"expected_owner":123456789,"expected_owner_type":true,"supplied_owner":"PRIVATE_OWNER_CANARY",
            "supplied_owner_type":"PRIVATE_TYPE_CANARY","raw_owner":"PRIVATE_RAW_CANARY","pages":17}),
        ),
        diagnostic(
            "support_discovery_v1",
            json!({"owner":123456789,"owner_type":"PRIVATE_TYPE_CANARY","zone_class":"PRIVATE_ZONE_CANARY",
            "primary_eligibility":"success","private_payload":"https://PRIVATE_PROVIDER_CANARY","listing_validated":true}),
        ),
        diagnostic(
            "support_missing_reference_v1",
            json!({"record_history":"first_seen", "lookup_scope":"PRIVATE_SCOPE_CANARY", "lookup_shape":123456789}),
        ),
    ]);
    let history = json!({"schema_version":1,"cycles_evicted":0,"queue_dropped":0,"groups_omitted":0,"previous_history_unavailable":false,"cycles":[current,old,malicious]});
    let path = data.join(format!("{}.support.json", sanitize_username(username)));
    std::fs::write(&path, serde_json::to_vec(&history).unwrap()).unwrap();
    let before = snapshot(&data);
    let output = root.path().join("support.json");
    clean_cmd()
        .env("KEI_DATA_DIR", &data)
        .arg("--config")
        .arg(&config)
        .arg("support-export")
        .arg("--output")
        .arg(&output)
        .assert()
        .success();
    assert_eq!(snapshot(&data), before);
    let bytes = std::fs::read(output).unwrap();
    assert!(bytes.len() <= 2 * 1024 * 1024);
    let text = std::str::from_utf8(&bytes).unwrap();
    for private in ["PRIVATE_", "123456789", username, path.to_str().unwrap()] {
        assert!(!text.contains(private), "{text}");
    }
    let bundle: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(bundle["history"]["status"], "available");
    let cycles = bundle["history"]["data"]["cycles"].as_array().unwrap();
    let events = cycles[0]["diagnostics"].as_array().unwrap();
    assert_eq!(events[0]["fields"]["expected_owner"], "absent");
    assert_ne!(events[0]["fields"], events[1]["fields"]);
    assert_eq!(events[2]["fields"]["supplied_owner"], "null");
    assert_eq!(events[3]["fields"]["supplied_owner_type"], "number");
    assert_eq!(events[4]["fields"]["primary_eligibility"], "eligible");
    assert_eq!(events[5]["fields"]["zone_class"], "other");
    assert_eq!(events[6]["fields"]["phase"], "primary_publication");
    assert_eq!(
        events[7]["fields"],
        json!({"record_history":"replacement", "prior_reference":"present",
        "sparse_marker":"true", "linkage":"partial_fields", "lookup_outcome":"live_reference", "lookup_family":"candidate",
        "lookup_scope":"matched", "lookup_shape":"live_reference"})
    );
    // Missing historical fields remain unavailable. They are never synthesized
    // into an observed absent owner or a false primary-publication decision.
    for key in [
        "expected_owner",
        "expected_owner_type",
        "supplied_owner",
        "supplied_owner_type",
    ] {
        assert!(cycles[1]["diagnostics"][0]["fields"].get(key).is_none());
    }
    for key in [
        "zone_class",
        "owner_type",
        "primary_eligibility",
        "listing_validated",
    ] {
        assert!(cycles[1]["diagnostics"][1]["fields"].get(key).is_none());
    }
    assert_eq!(cycles[1]["diagnostics"][1]["fields"]["owner"], "other");
    for key in ["lookup_scope", "lookup_shape"] {
        assert!(cycles[1]["diagnostics"][2]["fields"].get(key).is_none());
    }
    assert_eq!(
        cycles[2]["diagnostics"][2]["fields"],
        json!({"record_history":"first_seen"})
    );
    assert_eq!(cycles[1]["observations_total"], Value::Null);
    assert_eq!(cycles[2]["diagnostics"][0]["fields"], json!({"pages":17}));
    assert_eq!(
        cycles[2]["diagnostics"][1]["fields"],
        json!({"listing_validated":true})
    );
}
