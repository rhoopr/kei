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
    std::fs::write(
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
media = [\"photos\", \"videos\"]

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
    assert_eq!(json["version"], "3", "schema version");
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
    assert_eq!(json["stats"]["skipped"]["by_state"], 1);
    assert_eq!(json["stats"]["bytes_downloaded"], 4096);
    assert_eq!(json["stats"]["disk_bytes_written"], 4096);
    assert_eq!(json["stats"]["photos_downloaded"], 1);
    assert_eq!(json["stats"]["videos_downloaded"], 1);
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
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn status_shows_counts() {
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
    insert_asset(
        &conn,
        "a2",
        "downloaded",
        "photo2.jpg",
        Some("/p/photo2.jpg"),
        None,
        None,
    );
    insert_asset(
        &conn,
        "a3",
        "downloaded",
        "photo3.jpg",
        Some("/p/photo3.jpg"),
        None,
        None,
    );
    insert_asset(
        &conn,
        "a4",
        "failed",
        "photo4.jpg",
        None,
        Some("timeout"),
        None,
    );
    insert_asset(&conn, "a5", "pending", "photo5.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Total:      5"), "stdout: {stdout}");
    assert!(stdout.contains("Downloaded: 3"), "stdout: {stdout}");
    assert!(stdout.contains("Failed:     1"), "stdout: {stdout}");
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
}

#[test]
fn status_clean_run_notes_do_not_claim_complete_coverage() {
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
    conn.execute(
        "INSERT INTO asset_metadata_capture_revisions \
            (library, asset_id, revision, updated_at) \
         VALUES ('PrimarySync', 'a1', 1, ?1)",
        [1_700_000_000_i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, assets_seen, assets_downloaded,
            assets_failed, interrupted, enumeration_errors
         ) VALUES (?1, ?2, 'complete', 1, 1, 0, 0, 0)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "Recorded run/database notes: last sync recorded completion and no pending or failed catalog rows remain; this does not prove coverage"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_reports_metadata_capture_retry_without_private_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "private-legacy",
        "downloaded",
        "photo.jpg",
        Some("/p/photo.jpg"),
        None,
        None,
    );
    conn.execute("INSERT INTO metadata_capture_state(library,pending_revision,failed_assets,updated_at) VALUES ('PrimarySync',1,19,0)", []).unwrap();
    let evidence =
        serde_json::json!(["private-legacy", null, [["original", "abc", 1000, null]]]).to_string();
    conn.execute("INSERT INTO metadata_capture_retries VALUES ('PrimarySync','private-legacy',1,?1,1,1,?2,?3)", rusqlite::params![evidence,chrono::Utc::now().timestamp(),chrono::Utc::now().timestamp()+3600]).unwrap();
    conn.execute("INSERT INTO sync_runs(started_at,completed_at,status) VALUES (1700000000,1700000010,'complete')", []).unwrap();
    drop(conn);
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .arg("status")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 unresolved identities, 1 deferred retries"),
        "{stdout}"
    );
    assert!(stdout.contains("19 failed"), "{stdout}");
    assert!(stdout.contains("Recorded run/database notes:"), "{stdout}");
    assert!(!stdout.contains("private-legacy"));
}

#[test]
fn status_keeps_unresolved_identity_visible_after_another_zone_completes() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    conn.execute("INSERT INTO sync_runs (started_at, completed_at, status) VALUES (1700000000, 1700000010, 'complete')", []).unwrap();
    for (key, value) in [
        ("unresolved_current_asset_identity:PrimarySync", "1"),
        ("last_checkpoint_status", "current"),
        ("last_recovery_action", "none"),
        ("sync_token:v27:SharedSync-private", "shared-current"),
    ] {
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            [key, value],
        )
        .unwrap();
    }
    conn.execute("INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at,next_retry_at) VALUES ('PrimarySync','private-source',?1,?1,1,1700000000,?2)", rusqlite::params![r#"[1,"private-target","SharedSync-private","private-owner"]"#, chrono::Utc::now().timestamp()+3600]).unwrap();
    let output = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .arg("status")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .contains("Recorded run/database notes: unresolved asset identity in 1 provider zone"),
        "{stdout}"
    );
    assert!(stdout.contains("preserved"), "{stdout}");
    assert!(
        stdout
            .contains("Recorded sparse observations: 1 unresolved, 1 with a saved retry deadline"),
        "{stdout}"
    );
    for private in ["private-source", "private-target", "private-owner"] {
        assert!(!stdout.contains(private), "{stdout}");
    }
    assert!(!stdout.contains("SharedSync-private"), "{stdout}");
    conn.execute(
        "DELETE FROM metadata WHERE key = 'unresolved_current_asset_identity:PrimarySync'",
        [],
    )
    .unwrap();
    let output = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .arg("status")
        .assert()
        .success()
        .get_output()
        .clone();
    // A missing marker must not hide retained per-source work.
    assert!(String::from_utf8_lossy(&output.stdout).contains("Recorded run/database notes:"));
    conn.execute("DELETE FROM unresolved_sparse_identities", [])
        .unwrap();
    // Simulate actual resolution of both the observation and its marker.
    // Reopening an unclassified current row correctly reinstates that marker.
    conn.execute(
        "DELETE FROM metadata WHERE key='unresolved_current_asset_identity:PrimarySync'",
        [],
    )
    .unwrap();
    let output = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .arg("status")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Recorded run/database notes: last sync recorded completion"));
    assert!(stdout.contains("Current selected coverage: unknown offline"));
    assert!(!stdout.contains("Backup status: safe"));
    assert!(!stdout.contains("Recorded sparse observations:"));
}

#[test]
fn status_keeps_policy_excluded_assets_visible_without_coverage_claim() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "excluded",
        "policy_excluded",
        "old.mov",
        None,
        None,
        None,
    );
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, assets_seen, assets_downloaded,
            assets_failed, interrupted, enumeration_errors
         ) VALUES (?1, ?2, 'complete', 1, 0, 0, 0, 0)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Pending:    0"), "stdout: {stdout}");
    assert!(stdout.contains("Policy excluded: 1"), "stdout: {stdout}");
    assert!(
        stdout.contains(
            "Recorded run/database notes: last sync recorded completion and no pending or failed catalog rows remain; this does not prove coverage"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_shows_incomplete_run_notes_with_last_run_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "failed",
        "photo1.jpg",
        None,
        Some("timeout"),
        None,
    );
    insert_asset(&conn, "a2", "pending", "photo2.jpg", None, None, None);
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, assets_seen, assets_downloaded,
            assets_failed, interrupted, enumeration_errors
         ) VALUES (?1, ?2, 'complete', 2, 0, 1, 0, 2)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "Recorded run/database notes: 1 asset failed in the last sync; 2 enumeration errors occurred in the last sync; 1 failed asset remains; 1 pending asset remains"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_shows_api_total_and_inventory_warning() {
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
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, api_total_at_start,
            inventory_drop_detected, inventory_drop_previous_total,
            inventory_drop_current_total, inventory_drop_library
         ) VALUES (?1, ?2, 'complete', ?3, 1, ?4, ?5, ?6)",
        rusqlite::params![
            1_700_000_000_i64,
            1_700_000_010_i64,
            95_i64,
            100_i64,
            95_i64,
            "PrimarySync"
        ],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Last API total at start: 95"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(
            "Inventory warning: PrimarySync dropped 5 assets since the previous comparable full run (100 -> 95)"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_shows_partial_api_total() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, api_total_at_start,
            api_total_at_start_partial
         ) VALUES (?1, ?2, 'complete', ?3, 1)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64, 95_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Last API total at start: partial, 95"),
        "stdout: {stdout}"
    );
}

#[test]
fn status_prefers_running_sync_over_newer_completed_row() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    conn.execute(
        "INSERT INTO sync_runs (started_at, status) VALUES (?1, 'running')",
        rusqlite::params![1_700_000_000_i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sync_runs (started_at, completed_at, status) \
         VALUES (?1, ?2, 'complete')",
        rusqlite::params![1_700_000_030_i64, 1_700_000_040_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Sync in progress:   started 2023-11-14 22:13:20 UTC"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("Last sync completed:"),
        "active status must not imply the whole sync completed: {stdout}"
    );
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
}

#[test]
fn status_shows_full_enumeration_progress_marker() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        rusqlite::params!["enum_in_progress:PrimarySync", "1700000000"],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Full enumeration in progress: PrimarySync"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
}

#[test]
fn status_failed_shows_error_messages() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(
        &conn,
        "a1",
        "failed",
        "photo1.jpg",
        None,
        Some("connection reset"),
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--failed"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("connection reset"), "stdout: {stdout}");
}

#[test]
fn status_with_db_no_sync_runs() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Total:      1"), "stdout: {stdout}");
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
    // No "Last sync" lines since no sync_runs
    assert!(
        !stdout.contains("Last sync started"),
        "no sync runs, so no 'Last sync started', stdout: {stdout}"
    );
}

#[test]
fn status_failed_with_no_failures() {
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
        .args(["status", "--failed"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Failed:     0"), "stdout: {stdout}");
    // Should NOT print "Failed assets:" section
    assert!(
        !stdout.contains("Failed assets:"),
        "no failed assets section expected, stdout: {stdout}"
    );
}

#[test]
fn status_pending_shows_pending_assets() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    insert_asset(&conn, "a2", "pending", "photo2.jpg", None, None, None);
    insert_asset(
        &conn,
        "a3",
        "downloaded",
        "photo3.jpg",
        Some("/p/photo3.jpg"),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--pending"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Pending assets:"), "stdout: {stdout}");
    assert!(stdout.contains("photo1.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("photo2.jpg"), "stdout: {stdout}");
    // Downloaded asset must not appear in the pending listing
    assert!(!stdout.contains("photo3.jpg"), "stdout: {stdout}");
}

#[test]
fn status_downloaded_shows_downloaded_assets() {
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
    insert_asset(&conn, "a2", "pending", "photo2.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Downloaded assets:"), "stdout: {stdout}");
    assert!(stdout.contains("photo1.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("/p/photo1.jpg"), "stdout: {stdout}");
    // Pending asset must not appear in the downloaded listing
    assert!(!stdout.contains("photo2.jpg"), "stdout: {stdout}");
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
        .args(["status", "--pending"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Pending assets:"), "stdout: {stdout}");
}

#[test]
fn status_downloaded_with_null_local_path_surfaces_missing_marker() {
    // Covers the `<MISSING local_path>` display path in print_downloaded.
    // A downloaded row without a local_path is a state-DB invariant
    // violation; status must not silently hide it.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    // Directly insert a downloaded row with NULL local_path. insert_asset
    // helper would still pass None through, so we use it with explicit
    // Option::None for local_path.
    insert_asset(&conn, "a1", "downloaded", "broken.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("<MISSING local_path>"),
        "missing-path marker not surfaced: {stdout}"
    );
    assert!(stdout.contains("broken.jpg"), "stdout: {stdout}");
}

#[test]
fn status_all_three_flags_render_all_sections() {
    // End-to-end coverage for --failed --pending --downloaded combined.
    // Locks in the three-section rendering and proves the flags are
    // orthogonal in the actual binary (not just clap parsing).
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(
        &conn,
        "dl1",
        "downloaded",
        "dl.jpg",
        Some("/p/dl.jpg"),
        None,
        None,
    );
    insert_asset(&conn, "pend1", "pending", "pend.jpg", None, None, None);
    insert_asset(
        &conn,
        "fail1",
        "failed",
        "fail.jpg",
        None,
        Some("timeout"),
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--failed", "--pending", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Failed assets:"), "stdout: {stdout}");
    assert!(stdout.contains("fail.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("Pending assets:"), "stdout: {stdout}");
    assert!(stdout.contains("pend.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("Downloaded assets:"), "stdout: {stdout}");
    assert!(stdout.contains("dl.jpg"), "stdout: {stdout}");
}

#[test]
fn status_downloaded_paginates_past_page_size() {
    // Covers the pagination loop in run_status for --downloaded when the
    // result set exceeds page_size (100) but stays under the print cap
    // (200). 150 rows require at least two page fetches and all should
    // render (no truncation tail).
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..150 {
        let id = format!("dl{i:04}");
        let filename = format!("photo_{i:04}.jpg");
        let local = format!("/p/photo_{i:04}.jpg");
        insert_asset(
            &conn,
            &id,
            "downloaded",
            &filename,
            Some(&local),
            None,
            None,
        );
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Downloaded: 150"), "stdout: {stdout}");
    // First and last rows across the page boundary must both appear.
    assert!(stdout.contains("photo_0000.jpg"), "first row missing");
    assert!(stdout.contains("photo_0099.jpg"), "boundary row missing");
    assert!(
        stdout.contains("photo_0100.jpg"),
        "post-boundary row missing"
    );
    assert!(stdout.contains("photo_0149.jpg"), "last row missing");
    assert!(
        !stdout.contains("listing capped"),
        "no truncation tail expected when under cap; stdout: {stdout}"
    );
}

#[test]
fn status_downloaded_truncates_past_print_cap() {
    // Covers the 200-row listing cap for --downloaded on large libraries.
    // With 250 rows, the first 200 render and a tail names 50 more.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..250 {
        let id = format!("dl{i:04}");
        let filename = format!("photo_{i:04}.jpg");
        let local = format!("/p/photo_{i:04}.jpg");
        insert_asset(
            &conn,
            &id,
            "downloaded",
            &filename,
            Some(&local),
            None,
            None,
        );
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Downloaded: 250"), "stdout: {stdout}");
    assert!(stdout.contains("photo_0000.jpg"), "first row missing");
    assert!(stdout.contains("photo_0199.jpg"), "200th row missing");
    assert!(
        !stdout.contains("photo_0200.jpg"),
        "201st row should have been truncated; stdout: {stdout}"
    );
    assert!(
        stdout.contains("... and 50 more (listing capped at 200)"),
        "truncation tail missing; stdout: {stdout}"
    );
}

#[test]
fn status_failed_truncates_past_print_cap() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..250 {
        let id = format!("fail{i:04}");
        let filename = format!("photo_{i:04}.jpg");
        insert_asset(&conn, &id, "failed", &filename, None, Some("timeout"), None);
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--failed"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Failed:     250"), "stdout: {stdout}");
    assert!(
        stdout.contains("... and 50 more (listing capped at 200)"),
        "truncation tail missing; stdout: {stdout}"
    );
}

#[test]
fn status_retained_checkpoint_holds_show_actionable_redacted_guidance_after_other_zone_success() {
    use sha2::{Digest, Sha256};

    let private_cursor = "private-retained-cursor";
    let cursor_hash = format!("{:x}", Sha256::digest(private_cursor.as_bytes()));
    let hold = |attempt_count| {
        serde_json::json!({
            "version": 1,
            "prior_cursor_sha256": cursor_hash,
            "enum_config_hash": "private-requested-policy",
            "preservation_config_hash": null,
            "next_attempt_at": chrono::Utc::now().timestamp() + 3600,
            "attempt_count": attempt_count
        })
        .to_string()
    };
    for (encoded, action, guidance) in [
        (
            hold(1),
            "await_retained_checkpoint_evidence",
            "Recorded retained cursor was rejected. Preserve the state database and media. Current baseline and work evidence are reported separately; this recorded event does not establish an active retry or current sync hold.",
        ),
        (
            hold(3),
            "retained_checkpoint_retry_exhausted",
            "Recorded retained-checkpoint recovery exhausted its automatic attempts. Historical evidence remains preserved; this recorded event does not establish a current sync hold or missing selected content.",
        ),
        (
            format!("malformed-private-held-evidence:{private_cursor}"),
            "repair_retained_checkpoint_evidence",
            "Recorded retained-history recovery found malformed or unsupported evidence. Preserve the state database and media. This recorded event does not establish a current sync hold.",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let username = "test@example.com";
        let kept = dir.path().join("preserved.jpg");
        let bytes = include_bytes!("../data/media/pattern.jpg");
        std::fs::write(&kept, bytes).unwrap();
        let conn = create_state_db(dir.path(), username);
        insert_asset(
            &conn,
            "kept",
            "downloaded",
            "preserved.jpg",
            Some(kept.to_str().unwrap()),
            None,
            None,
        );
        conn.execute(
            "INSERT INTO asset_metadata_capture_revisions (library,asset_id,revision,updated_at) VALUES ('PrimarySync','kept',1,1700000000)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO sync_runs(started_at,completed_at,status) VALUES (1700000000,1700000010,'complete')",
            [],
        ).unwrap();
        for (key, value) in [
            ("sync_token:v27:PrimarySync", private_cursor),
            ("retained_checkpoint_hold:v27:PrimarySync", encoded.as_str()),
            // A separately successful zone has overwritten the global summary.
            // The durable per-zone hold must still control operator status.
            (
                "sync_token:v27:SharedSync-private",
                "private-healthy-zone-cursor",
            ),
            ("last_checkpoint_status", "current"),
            ("last_recovery_action", "none"),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES (?1,?2)",
                [key, value],
            )
            .unwrap();
        }
        drop(conn);

        let output = clean_cmd()
            .env("ICLOUD_USERNAME", username)
            .env("KEI_DATA_DIR", dir.path())
            .arg("status")
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout
                .lines()
                .any(|line| line == "Provider checkpoint: preserved"),
            "{action}: {stdout}"
        );
        assert!(
            !stdout.contains("Provider checkpoint: current"),
            "{action}: {stdout}"
        );
        assert!(stdout.contains("Recorded run/database notes: retained provider history is unavailable; this is not a current-selection coverage assessment"), "{action}: {stdout}");
        assert!(
            stdout
                .lines()
                .any(|line| line == format!("Last recovery action: {action}")),
            "{stdout}"
        );
        assert!(
            stdout.lines().any(|line| line.trim() == guidance),
            "{action}: {stdout}"
        );
        let diagnostics = format!("{stdout}{}", String::from_utf8_lossy(&output.stderr));
        for private in [
            private_cursor,
            cursor_hash.as_str(),
            "private-requested-policy",
            "private-healthy-zone-cursor",
            "SharedSync-private",
            "malformed-private-held-evidence",
        ] {
            assert!(
                !diagnostics.contains(private),
                "{action} exposed private evidence: {diagnostics}"
            );
        }
        assert_eq!(std::fs::read(&kept).unwrap(), bytes);
    }
}

#[test]
fn status_pending_reports_scoped_match_details_without_private_provider_keys() {
    let dir = tempfile::tempdir().unwrap();
    let username = "match-status@example.invalid";
    let conn = create_state_db(dir.path(), username);
    let diagnostic = serde_json::json!({
        "attempts":[{
            "generation":null,"pass":null,"child":"do-not-print-provider-child",
            "destination":{"encoding":"utf8","path":"/archive/selected.JPG"},
            "checksum":"do-not-print-provider-signature","observed_at":123,
            "explanation":{"candidates":[{
                "path":{"encoding":"utf8","path":"/archive/old\nname.JPG"},
                "reason":"unsupported_provider_proof_without_valid_receipt"
            }],"omitted":0}
        }],"omitted":0
    });
    insert_asset(
        &conn,
        "a1",
        "pending",
        "selected.JPG",
        None,
        Some(&format!("existing_match_v1:{diagnostic}")),
        None,
    );
    drop(conn);
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--pending"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("/archive/selected.JPG"));
    assert!(stdout.contains("old\\nname.JPG"));
    assert!(stdout.contains("unsupported_provider_proof_without_valid_receipt"));
    assert!(stdout.contains("stale or unbound"));
    assert!(stdout.contains("no safe force-trust"));
    assert!(!stdout.contains("do-not-print-provider"));
}

#[test]
fn status_reports_unknown_coverage_for_empty_and_historical_only_state() {
    let dir = tempfile::tempdir().unwrap();
    let username = "coverage@example.invalid";
    let conn = create_state_db(dir.path(), username);
    conn.execute("INSERT INTO sync_runs(started_at,completed_at,status) VALUES(1700000000,1700000010,'complete')",[]).unwrap();
    for historical in [false, true] {
        if historical {
            insert_asset(
                &conn,
                "legacy",
                "pending",
                "old.JPG",
                None,
                Some("historical identity unresolved"),
                None,
            );
            conn.execute("INSERT INTO metadata(key,value) VALUES('sync_token:v27:PrimarySync','opaque-saved-cursor')",[]).unwrap();
        }
        let out = clean_cmd()
            .env("ICLOUD_USERNAME", username)
            .env("KEI_DATA_DIR", dir.path())
            .arg("status")
            .assert()
            .success()
            .get_output()
            .clone();
        let output = String::from_utf8_lossy(&out.stdout);
        assert!(output.contains("Current selected coverage: unknown offline"));
        assert!(output.contains("Zero retained work does not mean a complete backup"));
        assert!(output.contains("Stored completed-media receipts: 0 validated"));
        assert!(!output.contains("Backup status: safe"));
        assert!(!output.contains("opaque-saved-cursor"));
        if historical {
            assert!(output.contains("1 pending asset remains"));
        }
    }
}

#[test]
fn status_scopes_saved_observation_and_refuses_stale_cursor() {
    use serde::Serialize;
    use sha2::{Digest, Sha256};
    #[derive(Serialize)]
    struct Plan {
        scope: String,
        config: String,
        profile: serde_json::Value,
        metadata: bool,
        flags: u8,
    }
    #[derive(Serialize)]
    struct Descriptor {
        version: u8,
        account: String,
        provider: String,
        library: String,
        plans: Vec<Plan>,
        cursor_hash: String,
        observed_at: i64,
        policy: [Option<String>; 2],
        admission_hash: String,
        proof_hash: String,
    }
    let dir = tempfile::tempdir().unwrap();
    let username = "observation@example.invalid";
    let conn = create_state_db(dir.path(), username);
    let (account, provider) = conn
        .query_row(
            "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let mut descriptor = Descriptor {
        version: 1,
        account,
        provider,
        library: "PrimarySync".into(),
        plans: vec![Plan {
            scope: serde_json::json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"private","zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}).to_string(),
            config: "synthetic-selected-plan".into(),
            profile: serde_json::json!({"current_download_metadata":1,"coverage":"selected_downloads","passes":[{"key":"pass","scope":{"database":"private","zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"kind":"unfiled"}}]}),
            metadata: false,
            flags: 0,
        }],
        cursor_hash: format!("{:x}", Sha256::digest(b"opaque-current-cursor")),
        observed_at: 1700000000,
        policy: [None, None],
        admission_hash: "a".repeat(64),
        proof_hash: String::new(),
    };
    // Serialization fixture for a completed empty synthetic observation. Actual
    // producer issuance and checkpoint atomicity are exercised by runner tests.
    descriptor.proof_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&descriptor).unwrap())
    );
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES('observed_selection:v27:PrimarySync',?1)",
        [serde_json::to_string(&descriptor).unwrap()],
    )
    .unwrap();
    for (token, certified) in [("opaque-current-cursor", true), ("changed-cursor", false)] {
        conn.execute("INSERT INTO metadata(key,value) VALUES('sync_token:v27:PrimarySync',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[token]).unwrap();
        let out = clean_cmd()
            .env("ICLOUD_USERNAME", username)
            .env("KEI_DATA_DIR", dir.path())
            .arg("status")
            .assert()
            .success()
            .get_output()
            .clone();
        let output = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            output.contains("complete as of this recorded observation"),
            certified
        );
        if certified {
            assert!(output.contains("Recorded Primary library selection"));
            assert!(output.contains("Other selected libraries or plans may lack proof"));
            assert!(output.contains("not unseen later changes"));
        } else {
            assert!(output.contains("no matching validated observation/checkpoint proof"));
        }
        assert!(!output.contains(token));
    }
    descriptor.plans[0].scope =
        serde_json::json!({"zone":{"zoneName":"SharedSync-other"}}).to_string();
    descriptor.proof_hash.clear();
    descriptor.proof_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&descriptor).unwrap())
    );
    conn.execute(
        "UPDATE metadata SET value=?1 WHERE key='observed_selection:v27:PrimarySync'",
        [serde_json::to_string(&descriptor).unwrap()],
    )
    .unwrap();
    conn.execute(
        "UPDATE metadata SET value='opaque-current-cursor' WHERE key='sync_token:v27:PrimarySync'",
        [],
    )
    .unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no matching validated observation/checkpoint proof",
        ));
}
