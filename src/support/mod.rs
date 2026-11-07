//! Privacy-safe offline support snapshot and ordinary redacted diagnostics.
#![allow(
    clippy::print_stdout,
    reason = "support-export prints its local result and review instructions"
)]
mod collect;
mod privacy;

pub(crate) use privacy::fixed_label;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

// Ok(None) means an omitted file; Err means its settings could not be read or parsed.
fn configuration(input: Result<Option<&crate::config::TomlConfig>, ()>, explicit: bool) -> Value {
    let threads = input.ok().and_then(|toml| {
        let download = toml.and_then(|t| t.download.as_ref());
        let bandwidth = download
            .and_then(|d| d.bandwidth_limit.as_deref())
            .map(crate::cli::parse_bandwidth_limit)
            .transpose()
            .ok()?;
        crate::config::resolve_download_threads(download.and_then(|d| d.threads), bandwidth).ok()
    });
    let toml = input.ok().flatten();
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
        "threads": threads,
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

/// Emits only typed, allowlisted diagnostic fields. This is ordinary tracing
/// output, not retained history, and never feeds sync, state, or checkpoint
/// decisions.
pub(crate) fn observe(kind: &'static str, fields: Value) {
    if let Some(fields) = fields
        .as_object()
        .and_then(|v| privacy::diagnostic(kind, v))
    {
        let fields = Value::Object(fields);
        tracing::debug!(diagnostic = kind, support_fields = %fields);
    }
}

#[cfg(test)]
pub(crate) mod test_capture;

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
    let partial = result?;
    println!(
        "Support snapshot saved: current local state only{}.",
        if partial {
            "; some evidence unavailable or truncated"
        } else {
            ""
        }
    );
    println!("Review the JSON file locally, then attach it if you choose. Nothing was uploaded.");
    Ok(())
}

/// Projects typed causes to fixed diagnostic fields without exposing error text.
pub(crate) fn classify_support_error(operation: &'static str, error: &anyhow::Error) -> Value {
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
        DownloadError::Other(error) => classify_support_error(operation, error),
    }
}

pub(crate) fn observe_metadata(kind: &'static str, fields: Value) {
    observe(kind, fields);
}

pub(crate) fn metadata_error(stage: &'static str, error: &anyhow::Error, committed: Option<bool>) {
    let mut fields = classify_support_error("metadata_capture", error);
    if let Some(map) = fields.as_object_mut() {
        map.insert("stage".into(), json!(stage));
        map.insert("publication_committed".into(), json!(committed));
    }
    observe_metadata("support_task_error_v1", fields);
}
