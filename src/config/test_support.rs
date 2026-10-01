//! Shared config test inputs.

use super::paths::GlobalArgs;
use super::runtime::Config;
use crate::cli::SyncArgs;

pub(super) fn default_globals() -> GlobalArgs {
    GlobalArgs {
        username: Some("u@example.com".to_string()),
        domain: None,
        data_dir: None,
    }
}

pub(super) fn default_password() -> crate::cli::PasswordArgs {
    crate::cli::PasswordArgs::default()
}

pub(super) fn default_sync() -> SyncArgs {
    SyncArgs::default()
}

pub(super) fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

pub(super) fn assert_album_raw(config: &Config, expected: &[&str]) {
    assert_eq!(config.filters.selection.albums.to_raw(), strings(expected));
}
