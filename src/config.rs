//! Runtime configuration facade. Preserves existing config paths and visibility.
//!
//! `input` owns TOML schema and loading, `runtime` owns resolved policy types,
//! and `resolve` assembles policy with the existing precedence rules. `paths`
//! owns bootstrap and credential paths, `templates` validates folder tokens,
//! and `persistence` writes the supported TOML projection.

mod input;
mod paths;
mod persistence;
mod resolve;
mod runtime;
mod templates;

#[cfg(test)]
mod test_support;

#[expect(
    unused_imports,
    reason = "preserve existing config facade paths and visibility"
)]
pub(crate) use input::{
    TomlAuth, TomlConfig, TomlDownload, TomlFilters, TomlImport, TomlMetadata, TomlNotifications,
    TomlPhotos, TomlReport, TomlRetry, TomlServer, TomlUi, TomlWatch, load_toml_config,
};
#[expect(
    unused_imports,
    reason = "preserve existing config facade paths and visibility"
)]
pub(crate) use paths::{
    GlobalArgs, default_config_path, default_cookie_dir, expand_tilde, kei_data_dir,
    kei_data_dir_with_home, resolve_data_dir, resolve_password_command, resolve_password_file,
    validate_download_dir,
};
pub(crate) use persistence::persist_first_run_config;
#[expect(
    unused_imports,
    reason = "preserve existing config facade paths and visibility"
)]
pub(crate) use resolve::{
    PathDerivationCliArgs, PathDerivationFields, SyncConfigOverrides, resolve_auth,
    resolve_library_selector, resolve_media_selection, resolve_notify_systemd,
    resolve_path_derivation_fields, smart_retry_delay, unfiled_default,
};
#[expect(
    unused_imports,
    reason = "preserve existing config facade paths and visibility"
)]
pub use runtime::{
    AuthConfig, Config, CreatedDateFilter, DownloadSettings, FilterConfig, ImportConfig,
    MetadataConfig, NotificationConfig, PhotoConfig, ReportConfig, ResolvedRetryConfig,
    RuntimeConfig, ServerConfig, UiConfig, WatchConfig,
};
pub(crate) use runtime::{MediaKind, MediaSelection, parse_created_date_filter};
#[cfg_attr(
    not(test),
    expect(
        unused_imports,
        reason = "preserve existing config facade paths and visibility"
    )
)]
pub(crate) use templates::{
    DEFAULT_FOLDER_STRUCTURE_ALBUMS, DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
};
