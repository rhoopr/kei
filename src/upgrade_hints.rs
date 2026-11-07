pub(crate) const CONFIG_EXAMPLE_URL: &str =
    "https://github.com/rhoopr/kei/blob/main/example.config.toml";
pub(crate) const ACCOUNT_STATE_MIGRATION_URL: &str =
    "https://github.com/rhoopr/kei/blob/main/docs/account-state-migration.md";

pub(crate) const REMOVED_DURABLE_ENV_VARS: &[&str] = &[
    "KEI_DOWNLOAD_DIR",
    "KEI_DIRECTORY",
    "KEI_FOLDER_STRUCTURE",
    "KEI_FOLDER_STRUCTURE_ALBUMS",
    "KEI_FOLDER_STRUCTURE_SMART_FOLDERS",
    "KEI_ALBUM",
    "KEI_EXCLUDE_ALBUM",
    "KEI_LIBRARY",
    "KEI_SKIP_VIDEOS",
    "KEI_SKIP_PHOTOS",
    "KEI_THREADS",
    "KEI_THREADS_NUM",
    "KEI_BANDWIDTH_LIMIT",
    "KEI_TEMP_SUFFIX",
    "KEI_MAX_RETRIES",
    "KEI_MAX_DOWNLOAD_ATTEMPTS",
    "KEI_WATCH_WITH_INTERVAL",
    "KEI_NOTIFY_SYSTEMD",
    "KEI_PID_FILE",
    "KEI_RECONCILE_EVERY_N_CYCLES",
    "KEI_NOTIFICATION_SCRIPT",
    "KEI_REPORT_JSON",
    "KEI_METRICS_PORT",
];

pub(crate) fn present_removed_durable_env_vars() -> Vec<&'static str> {
    REMOVED_DURABLE_ENV_VARS
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some())
        .collect()
}

pub(crate) fn stale_env_hint() -> Option<String> {
    let present = present_removed_durable_env_vars();
    if present.is_empty() {
        return None;
    }

    Some(format!(
        "note: found removed v0.20 env config: {}.\n      These env vars are ignored in v0.20. Move durable sync settings into config.toml.\n      Current settings: {CONFIG_EXAMPLE_URL}\n      Legacy state adoption: {ACCOUNT_STATE_MIGRATION_URL}",
        present.join(", ")
    ))
}

pub(crate) fn with_stale_env_hint(mut message: String) -> String {
    if let Some(hint) = stale_env_hint() {
        message.push_str("\n\n");
        message.push_str(&hint);
    }
    message
}

pub(crate) fn toml_unknown_field_hint() -> String {
    format!(
        "note: this config contains a key kei does not recognize. If this is a pre-v0.20 config, move durable sync settings to the current config.toml shape.\n      Current settings: {CONFIG_EXAMPLE_URL}\n      Legacy state adoption: {ACCOUNT_STATE_MIGRATION_URL}"
    )
}
