//! One bounded selection for all Rust live entry points.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const SELECTION: &str = include_str!("../data/live-selection.toml");

pub fn live_recent() -> u32 {
    let table: toml::Table = SELECTION.parse().expect("shared live selection TOML");
    u32::try_from(
        table
            .get("import_sample")
            .expect("import sample")
            .get("recent")
            .expect("recent")
            .as_integer()
            .expect("recent count"),
    )
    .expect("bounded positive live count")
}

pub fn live_config(body: &str) -> String {
    live_config_with_bound(body, &require_fixed_date())
}

fn fixed_date(value: Option<&str>) -> Result<&str, &'static str> {
    let value = value.ok_or(
        "Set KEI_LIVE_SKIP_CREATED_BEFORE to an explicit fixed YYYY-MM-DD before live tests",
    )?;
    if value.len() != 10 || chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err() {
        return Err(
            "KEI_LIVE_SKIP_CREATED_BEFORE must be a fixed YYYY-MM-DD, not a rolling window",
        );
    }
    Ok(value)
}

pub fn require_fixed_date() -> String {
    let value = std::env::var("KEI_LIVE_SKIP_CREATED_BEFORE").ok();
    fixed_date(value.as_deref())
        .expect("bounded live selection required")
        .to_owned()
}

fn live_config_with_bound(body: &str, bound: &str) -> String {
    let mut config: toml::Table = body.parse().expect("live scenario TOML");
    let defaults: toml::Table = SELECTION.parse().expect("shared live selection TOML");
    let filters = config
        .entry("filters")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .expect("filters table");
    for (key, value) in defaults
        .get("filters")
        .expect("filters")
        .as_table()
        .expect("default filters")
    {
        filters.entry(key.clone()).or_insert_with(|| value.clone());
    }
    assert!(
        !filters.contains_key("recent") && !filters.contains_key("recent_scope"),
        "retired sync count in live fixture"
    );
    let selected_bound = filters
        .get("skip_created_before")
        .map_or(bound, |value| value.as_str().expect("fixed live date"));
    fixed_date(Some(selected_bound)).expect("fixed live date");
    filters
        .entry("skip_created_before")
        .or_insert_with(|| toml::Value::String(bound.to_owned()));
    toml::to_string(&config).expect("serialize bounded live config")
}

pub fn write_live_config(dir: &Path, name: &str, body: &str) -> PathBuf {
    super::write_toml_config(dir, name, &live_config(body))
}

fn eligible_inventory(stdout: &[u8]) -> Result<usize, String> {
    let count = String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    if count == 0 {
        return Err("Empty bounded live selection: observed eligible filenames=0. Select a fixed capture-date range containing at least one eligible asset before running live tests.".to_owned());
    }
    Ok(count)
}

pub(super) fn ensure_selection(username: &str, password: &str, cookies: &Path) {
    static CHECKED: OnceLock<()> = OnceLock::new();
    CHECKED.get_or_init(|| {
        let data = tempfile::tempdir().expect("selection preflight state");
        let config = write_live_config(
            data.path(),
            "selection-preflight",
            &format!("[download]\ndirectory = {}\n", super::toml_string(&data.path().join("media").to_string_lossy())),
        );
        super::with_auth_retry(|| {
            super::copy_auth_material(username, cookies, data.path());
            let output = super::cmd()
                .env("ICLOUD_USERNAME", username)
                .env("KEI_DATA_DIR", data.path())
                .args(["sync", "--password", password, "--config", config.to_str().expect("config path"), "--only-print-filenames", "--no-progress-bar"])
                .timeout(Duration::from_secs(180))
                .assert().success().get_output().clone();
            let count = eligible_inventory(&output.stdout).expect("live selection preflight");
            eprintln!("Live selection: primary library, albums=none, unfiled=true, fixed lower date={}, eligible filenames={count}", require_fixed_date());
        });
    });
}

#[test]
fn shared_live_selection_is_bounded_and_preserves_scenario_options() {
    let value: toml::Table = live_config_with_bound("[download]\nthreads = 2\n", "2025-01-02")
        .parse()
        .unwrap();
    assert_eq!(
        value
            .get("filters")
            .unwrap()
            .get("albums")
            .unwrap()
            .as_array()
            .unwrap(),
        &[toml::Value::String("none".into())]
    );
    assert_eq!(
        value
            .get("filters")
            .unwrap()
            .get("libraries")
            .unwrap()
            .as_array()
            .unwrap(),
        &[toml::Value::String("primary".into())]
    );
    assert_eq!(
        value
            .get("filters")
            .unwrap()
            .get("unfiled")
            .unwrap()
            .as_bool(),
        Some(true)
    );
    assert_eq!(
        value
            .get("filters")
            .unwrap()
            .get("skip_created_before")
            .unwrap()
            .as_str(),
        Some("2025-01-02")
    );
    assert_eq!(
        value
            .get("download")
            .unwrap()
            .get("threads")
            .unwrap()
            .as_integer(),
        Some(2)
    );
    assert!(live_recent() > 0 && live_recent() <= 10);
}

#[test]
fn live_sync_requires_an_explicit_fixed_date() {
    for value in [
        None,
        Some(""),
        Some("30d"),
        Some("2025-02-30"),
        Some("2025-1-2"),
    ] {
        assert!(fixed_date(value).is_err(), "{value:?}");
    }
    assert_eq!(fixed_date(Some("2025-01-02")), Ok("2025-01-02"));
}

#[test]
fn live_preflight_requires_nonempty_inventory() {
    assert_eq!(eligible_inventory(b"one.JPG\n\ntwo.MOV\n"), Ok(2));
    for empty in [b"".as_slice(), b" \n\t\n".as_slice()] {
        assert!(
            eligible_inventory(empty)
                .unwrap_err()
                .contains("observed eligible filenames=0")
        );
    }
}
