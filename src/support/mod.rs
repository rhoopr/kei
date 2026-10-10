//! Privacy-safe offline support export and bounded normal-operation evidence.
#![allow(
    clippy::print_stdout,
    reason = "support-export prints its local result and review instructions"
)]
mod collect;
pub(crate) mod history;
mod privacy;

pub(crate) use history::{begin, complete, observe, observe_scoped};
pub(crate) use privacy::fixed_label;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) fn history_path(directory: &Path, username: &str, realm: &str) -> PathBuf {
    directory.join(format!(
        "{}.support.json",
        crate::account::namespace(username, realm)
    ))
}

pub(crate) fn initialize(
    globals: &crate::config::GlobalArgs,
    toml: Option<&crate::config::TomlConfig>,
    explicit: bool,
) -> Option<(history::Recorder, history::Guard)> {
    let (username, _, domain, directory) =
        crate::config::resolve_auth(globals, &crate::cli::PasswordArgs::default(), toml);
    if username.is_empty() {
        return None;
    }
    let configuration = configuration(toml, explicit);
    history::start(
        history_path(&directory, &username, domain.as_str()),
        configuration,
    )
}

pub(crate) fn runtime_configuration(config: &crate::config::Config) -> Value {
    let mut value =
        serde_json::to_value(crate::report::RunOptions::from_config(config)).unwrap_or(Value::Null);
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "filename_exclusions".into(),
            json!(config.download.filename_exclude.len()),
        );
        map.insert("watch_interval_secs".into(), json!(config.watch.interval));
        map.insert("per_transfer".into(), json!(config.retry.max_retries));
        map.insert(
            "per_asset".into(),
            json!(config.retry.max_download_attempts),
        );
        map.insert(
            "legacy_preservation_allow_hardlinks".into(),
            json!(config.download.legacy_preservation_allow_hardlinks),
        );
        map.insert(
            "keep_unicode_in_filenames".into(),
            json!(config.photos.keep_unicode_in_filenames),
        );
        map.insert(
            "date_filter_present".into(),
            json!(
                config.filters.skip_created_before.is_some()
                    || config.filters.skip_created_after.is_some()
            ),
        );
        map.insert(
            "recent_limit_present".into(),
            json!(config.filters.recent.is_some()),
        );
        map.insert(
            "report_configured".into(),
            json!(config.report.json.is_some()),
        );
        map.insert("strict".into(), json!(config.import.strict));
        map.insert("xmp_feature".into(), json!(cfg!(feature = "xmp")));
        use crate::selection::{AlbumSelector, SmartFolderSelector};
        let (mode, included, excluded) = match &config.filters.selection.albums {
            AlbumSelector::None => ("none", 0, 0),
            AlbumSelector::All { excluded } => ("all", 0, excluded.len()),
            AlbumSelector::Named { included, excluded } => {
                ("named", included.len(), excluded.len())
            }
        };
        map.insert("album_mode".into(), json!(mode));
        map.insert("album_selectors".into(), json!(included));
        map.insert("album_exclusions".into(), json!(excluded));
        let (mode, included, excluded, sensitive) = match &config.filters.selection.smart_folders {
            SmartFolderSelector::None => ("none", 0, 0, false),
            SmartFolderSelector::All {
                excluded,
                include_sensitive,
            } => ("all", 0, excluded.len(), *include_sensitive),
            SmartFolderSelector::Named { included, excluded } => {
                ("named", included.len(), excluded.len(), false)
            }
        };
        map.insert("smart_folder_mode".into(), json!(mode));
        map.insert("smart_folder_selectors".into(), json!(included));
        map.insert("smart_folder_exclusions".into(), json!(excluded));
        map.insert("sensitive_folders".into(), json!(sensitive));
        map.insert(
            "library_selectors".into(),
            json!(config.filters.selection.libraries.named.len()),
        );
        map.insert(
            "library_exclusions".into(),
            json!(config.filters.selection.libraries.excluded.len()),
        );
        map.insert(
            "primary_library".into(),
            json!(config.filters.selection.libraries.primary),
        );
        map.insert(
            "shared_libraries".into(),
            json!(config.filters.selection.libraries.shared_all),
        );
        map.insert("unfiled".into(), json!(config.filters.selection.unfiled));
    }
    privacy::configuration(&value)
}

fn configuration(toml: Option<&crate::config::TomlConfig>, explicit: bool) -> Value {
    let filters = toml.and_then(|t| t.filters.as_ref());
    let download = toml.and_then(|t| t.download.as_ref());
    let auth = toml.and_then(|t| t.auth.as_ref());
    let watch = toml.and_then(|t| t.watch.as_ref());
    let mut value = json!({
        "config_explicit": explicit,
        "data_dir_from_environment": std::env::var_os("KEI_DATA_DIR").is_some(),
        "username_from_environment": std::env::var_os("ICLOUD_USERNAME").is_some(),
        "data_dir_configured": toml.is_some_and(|t| t.data_dir.is_some()),
        "password_command_configured": auth.is_some_and(|a| a.password_command.is_some()),
        "password_file_configured": auth.is_some_and(|a| a.password_file.is_some()),
        "album_selectors": filters.and_then(|f| f.albums.as_ref()).map_or(0, Vec::len),
        "smart_folder_selectors": filters.and_then(|f| f.smart_folders.as_ref()).map_or(0, Vec::len),
        "library_selectors": filters.and_then(|f| f.libraries.as_ref()).map_or(0, Vec::len),
        "filename_exclusions": filters.and_then(|f| f.filename_exclude.as_ref()).map_or(0, Vec::len),
        "unfiled": filters.and_then(|f| f.unfiled).unwrap_or(true),
        "date_filter_present": filters.is_some_and(|f| f.skip_created_before.is_some() || f.skip_created_after.is_some()),
        "recent_limit_present": filters.is_some_and(|f| f.recent.is_some()),
        "threads": download.and_then(|d| d.threads).unwrap_or(3),
        "legacy_preservation_allow_hardlinks": download.and_then(|d| d.legacy_preservation_allow_hardlinks).unwrap_or(false),
        "watch_interval_secs": watch.and_then(|w| w.interval),
        "reconcile_every_n_cycles": watch.and_then(|w| w.reconcile_every_n_cycles),
        "report_configured": toml.and_then(|t| t.report.as_ref()).is_some_and(|r| r.json.is_some()),
        "notification_configured": toml.and_then(|t| t.notifications.as_ref()).is_some_and(|n| n.script.is_some()),
        "server_configured": toml.is_some_and(|t| t.server.is_some()),
        "folder_template_custom": download.is_some_and(|d| d.folder_structure.is_some()),
        "album_template_custom": download.is_some_and(|d| d.folder_structure_albums.is_some()),
        "smart_template_custom": download.is_some_and(|d| d.folder_structure_smart_folders.is_some()),
        "xmp_feature": cfg!(feature = "xmp"),
    });
    // Input enums are already parsed into typed values, but the wire contract
    // remains an explicit projection rather than serializing the TOML object.
    if let Some(photos) = toml.and_then(|t| t.photos.as_ref())
        && let Ok(v) = serde_json::to_value(photos)
        && let (Some(dst), Some(src)) = (value.as_object_mut(), v.as_object())
    {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    privacy::configuration(&value)
}

pub(crate) async fn export(
    args: crate::cli::SupportExportArgs,
    raw_config: &str,
) -> anyhow::Result<()> {
    let config_path = crate::config::expand_tilde(raw_config);
    let config_path = if raw_config == "~/.config/kei/config.toml"
        && !config_path.exists()
        && Path::new("/config/config.toml").is_file()
    {
        PathBuf::from("/config/config.toml")
    } else {
        config_path
    };
    let explicit = raw_config != "~/.config/kei/config.toml" && raw_config != "/config/config.toml";
    let output = args.output;
    let result =
        tokio::task::spawn_blocking(move || collect::export(&config_path, explicit, &output))
            .await?;
    let (cycles, partial) = result?;
    println!(
        "Support export saved: {cycles} retained operation/cycle records{}.",
        if partial {
            "; some evidence unavailable or truncated"
        } else {
            ""
        }
    );
    println!("Review the JSON file locally, then attach it if you choose. Nothing was uploaded.");
    Ok(())
}

pub(crate) fn error_fields(operation: &'static str, error: &anyhow::Error) -> Value {
    let mut class = "other";
    let mut errno = None;
    let mut http_status = None;
    let mut sqlite_code = None;
    for cause in error.chain() {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            cause.downcast_ref::<rusqlite::Error>()
        {
            class = "state";
            sqlite_code = Some(code.extended_code);
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            errno = io.raw_os_error();
            class = match io.kind() {
                std::io::ErrorKind::PermissionDenied => "permission_denied",
                std::io::ErrorKind::NotFound => "not_found",
                std::io::ErrorKind::AlreadyExists => "already_exists",
                std::io::ErrorKind::Unsupported => "unsupported",
                std::io::ErrorKind::OutOfMemory => "out_of_memory",
                _ => "io",
            };
        }
        if let Some(download) = cause.downcast_ref::<crate::download::error::DownloadError>() {
            match download {
                crate::download::error::DownloadError::HttpStatus { status, .. }
                | crate::download::error::DownloadError::Http { status, .. } => {
                    http_status = Some(*status);
                }
                crate::download::error::DownloadError::Disk(io) => {
                    errno = io.raw_os_error();
                    class = "io";
                }
                crate::download::error::DownloadError::Interrupted { .. } => {
                    class = "interrupted";
                }
                _ => {}
            }
        }
        if let Some(icloud) = cause.downcast_ref::<crate::icloud::error::ICloudError>() {
            match icloud {
                crate::icloud::error::ICloudError::ServiceNotActivated { .. } => {
                    class = "provider_not_ready"
                }
                crate::icloud::error::ICloudError::SessionExpired { status } => {
                    http_status = Some(*status)
                }
                crate::icloud::error::ICloudError::MisdirectedRequest => http_status = Some(421),
                _ => {}
            }
        }
        if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
            http_status = request.status().map(|s| s.as_u16());
        }
    }
    if matches!(http_status, Some(401 | 403 | 421)) {
        class = "authentication";
    } else if matches!(http_status, Some(429 | 503)) {
        class = "rate_limited";
    } else if http_status == Some(410) {
        class = "expired_url";
    }
    json!({ "operation": operation, "class": class, "errno": errno, "http_status": http_status, "sqlite_code": sqlite_code })
}

#[cfg(test)]
mod tests;

pub(crate) fn download_error_fields(
    operation: &'static str,
    error: &crate::download::error::DownloadError,
) -> Value {
    use crate::download::error::DownloadError;
    match error {
        DownloadError::HttpStatus { status, .. } | DownloadError::Http { status, .. } => json!({
            "operation": operation, "http_status": status,
            "class": if error.is_session_expired() { "authentication" } else if error.is_expired_url() { "expired_url" }
                else if error.is_rate_limited() { "rate_limited" } else { "request_failed" },
        }),
        DownloadError::Disk(io) => {
            json!({"operation": operation, "class": "io", "errno": io.raw_os_error()})
        }
        DownloadError::Interrupted { .. } => {
            json!({"operation": operation, "class": "interrupted"})
        }
        DownloadError::ContentLengthMismatch { .. } => {
            json!({"operation": operation, "class": "size_mismatch"})
        }
        DownloadError::InvalidContent { .. } => {
            json!({"operation": operation, "class": "invalid_content"})
        }
        DownloadError::Other(error) => error_fields(operation, error),
    }
}
