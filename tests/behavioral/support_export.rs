//! Real offline command boundary: no auth, credential commands, state migration or media scans.
use super::support::{clean_cmd, sanitize_username};
use serde_json::Value;
use std::path::Path;

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
