//! One bounded selection for all Rust live entry points.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const SELECTION: &str = include_str!("../data/live-selection.toml");

pub fn live_recent() -> u32 {
    let table: toml::Table = SELECTION.parse().expect("shared live selection TOML");
    u32::try_from(
        table
            .get("filters")
            .expect("filters")
            .get("recent")
            .expect("recent")
            .as_integer()
            .expect("recent count"),
    )
    .expect("bounded positive live count")
}

pub fn live_config(body: &str) -> String {
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
        return Err(format!(
            "Empty bounded live selection: primary library, recent={}; observed eligible filenames=0. Add at least one eligible asset before running live tests.",
            live_recent()
        ));
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
            eprintln!("Live selection: primary library, albums=none, unfiled=true, recent={}, eligible filenames={count}", live_recent());
        });
    });
}

#[test]
fn shared_live_selection_is_bounded_and_preserves_scenario_options() {
    let value: toml::Table = live_config("[download]\nthreads = 2\n[filters]\nrecent = 3\n")
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
            .get("recent")
            .unwrap()
            .as_integer(),
        Some(3)
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
