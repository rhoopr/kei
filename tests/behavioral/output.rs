//! Read-only status output and the binary JSON report boundary.

use super::{clean_cmd, create_state_db, insert_asset};
#[cfg(debug_assertions)]
use crate::common;
use predicates::prelude::predicate;

#[cfg(debug_assertions)]
#[test]
fn sync_report_json_offline_binary_boundary_writes_current_schema() {
    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("photos");
    let report_path = dir.path().join("sync_report.json");
    let config_path = dir.path().join("config.toml");
    let username = "offline-report@example.com";
    common::write_versioned_config(
        &config_path,
        format!(
            "\
[auth]
username = {username}

[download]
directory = {download_dir}
folder_structure = \"%Y/%m/%d\"
threads = 3

[filters]
exclude = [\"live-photo-stills\", \"live-photo-videos\"]

[report]
json = {report_path}

[ui]
friendly = false
",
            username = common::toml_string(username),
            download_dir = common::toml_string(&download_dir.to_string_lossy()),
            report_path = common::toml_string(&report_path.to_string_lossy()),
        ),
    )
    .unwrap();

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kei"))
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env_remove("KEI_CONFIG")
        .env_remove("KEI_DATA_DIR")
        .env_remove("KEI_DOWNLOAD_DIR")
        .env_remove("KEI_LOG_LEVEL")
        .env_remove("KEI_NO_AUTO_CONFIG")
        .env("KEI_DATA_DIR", &data_dir)
        .env("KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS", "1")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .output()
        .expect("run kei");
    assert!(
        output.status.success(),
        "kei sync failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let body = std::fs::read_to_string(&report_path).expect("sync_report.json");
    let json: serde_json::Value = serde_json::from_str(&body).expect("valid report JSON");
    assert_eq!(json["version"], "5", "schema version");
    assert!(json["kei_version"].as_str().is_some_and(|v| !v.is_empty()));
    assert!(
        json["timestamp"]
            .as_str()
            .is_some_and(|ts| chrono::DateTime::parse_from_rfc3339(ts).is_ok()),
        "timestamp must be RFC3339: {}",
        json["timestamp"]
    );
    assert_eq!(json["status"], "success");
    assert_eq!(json["options"]["username"], username);
    assert_eq!(
        json["options"]["download_dir"],
        download_dir.display().to_string()
    );
    assert_eq!(json["options"]["folder_structure"], "%Y/%m/%d");
    assert_eq!(json["options"]["threads"], 3);
    assert_eq!(
        json["options"]["media"],
        serde_json::json!(["photos", "videos"])
    );
    assert_eq!(json["options"]["dry_run"], false);
    assert_eq!(json["stats"]["assets_seen"], 3);
    assert_eq!(json["stats"]["downloaded"], 2);
    assert!(
        json.get("failed_assets").is_none(),
        "clean success report should omit failed_assets: {json}"
    );
}

#[test]
fn status_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}
#[test]
fn status_pending_empty_when_none_pending() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some("/p/photo1.jpg"),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
        .args(["status", "--pending"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Pending assets:"), "stdout: {stdout}");
}
