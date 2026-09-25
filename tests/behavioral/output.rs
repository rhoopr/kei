//! Read-only status output and the binary JSON report boundary.

use super::{clean_cmd, create_state_db, insert_asset};
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
fn status_shows_safe_backup_summary_after_clean_sync() {
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
            "Backup status: safe - last sync completed and no pending or failed assets are recorded"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_keeps_unresolved_identity_unsafe_after_another_zone_completes() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    conn.execute("INSERT INTO sync_runs (started_at, completed_at, status) VALUES (1700000000, 1700000010, 'complete')", []).unwrap();
    for (key, value) in [
        ("unresolved_asset_identity:PrimarySync", "1"),
        ("last_checkpoint_status", "current"),
        ("last_recovery_action", "none"),
        ("sync_token:SharedSync-private", "shared-current"),
    ] {
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            [key, value],
        )
        .unwrap();
    }
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
        stdout.contains("Backup status: unsafe - unresolved asset identity in 1 provider zone"),
        "{stdout}"
    );
    assert!(stdout.contains("preserved"), "{stdout}");
    assert!(!stdout.contains("SharedSync-private"), "{stdout}");
    conn.execute(
        "DELETE FROM metadata WHERE key = 'unresolved_asset_identity:PrimarySync'",
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("Backup status: safe"));
}

#[test]
fn status_treats_policy_excluded_assets_as_safe_and_visible() {
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
            "Backup status: safe - last sync completed and no pending or failed assets are recorded"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_shows_unsafe_backup_summary_with_last_run_reasons() {
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
            "Backup status: unsafe - 1 asset failed in the last sync; 2 enumeration errors occurred in the last sync; 1 failed asset remains; 1 pending asset remains"
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
