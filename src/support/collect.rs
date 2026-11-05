//! Collection reads only fixed local inputs. No migration, credentials or provider objects.
use super::configuration;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::Path;

const INPUT_BYTES: u64 = 1024 * 1024;
const OUTPUT_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn read_json(path: &Path, limit: u64) -> Option<Value> {
    serde_json::from_slice(&read_bytes(path, limit)?).ok()
}
fn read_bytes(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

pub(super) fn export(config_path: &Path, explicit: bool, output: &Path) -> anyhow::Result<bool> {
    let bytes = read_bytes(config_path, INPUT_BYTES);
    let toml = bytes
        .as_deref()
        .and_then(|b| std::str::from_utf8(b).ok())
        .and_then(|s| toml::from_str::<crate::config::TomlConfig>(s).ok());
    let config_status = if toml.is_some() {
        "available"
    } else if config_path.exists() {
        "invalid_or_unreadable"
    } else {
        "unavailable"
    };
    let globals = crate::config::GlobalArgs::from_bootstrap_env();
    let (username, _, domain, directory) = crate::config::resolve_auth(
        &globals,
        &crate::cli::PasswordArgs::default(),
        toml.as_ref(),
    );
    let state = if username.is_empty() {
        json!({"status": "account_unavailable"})
    } else {
        crate::state::db::support::collect(&directory, &username, domain.as_str())
    };
    let health = read_json(&directory.join("health.json"), 32 * 1024).map(|v| {
        let mut safe = super::privacy::numbers(
            &v,
            &[
                "consecutive_failures",
                "total_syncs",
                "total_failures",
                "unattributed_legacy_assets",
                "unattributed_legacy_pending",
            ],
        );
        for key in ["last_sync_at", "last_success_at"] {
            if let Some(timestamp) = v
                .get(key)
                .and_then(Value::as_str)
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                && let Some(map) = safe.as_object_mut()
            {
                map.insert(
                    key.into(),
                    json!(timestamp.with_timezone(&chrono::Utc).to_rfc3339()),
                );
            }
        }
        safe
    });
    let filesystem = filesystem(
        toml.as_ref()
            .and_then(|t| t.download.as_ref())
            .and_then(|d| d.directory.as_deref()),
    );
    let partial = config_status != "available"
        || state.get("status").and_then(Value::as_str) != Some("available")
        || health.is_none()
        || state.get("complete").and_then(Value::as_bool) == Some(false);
    let bundle = json!({
        "schema_version": 1,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "privacy_contract": "allowlist_v1",
        "summary": {"retained_records": 0, "partial": partial,
            "collection": "offline_read_only", "automatic_upload": false},
        "build": {
            "version": env!("CARGO_PKG_VERSION"), "revision": option_env!("KEI_BUILD_REVISION").unwrap_or("unavailable"),
            "source_dirty": option_env!("KEI_BUILD_DIRTY").unwrap_or("unavailable"),
            "target_os": std::env::consts::OS, "target_arch": std::env::consts::ARCH,
            "xmp_feature": cfg!(feature = "xmp"), "debug_assertions": cfg!(debug_assertions),
            "install_method": install_method(), "docker_digest": "unavailable",
            "supported_state_schema": crate::state::schema::SCHEMA_VERSION,
        },
        "configuration": {"status": config_status, "scope": "current_file_and_bootstrap_environment",
            "values": configuration(toml.as_ref(), explicit),
            "effective_sync_overrides": "unavailable_after_recorder_removal"},
        "state": state,
        "health": {"status": if health.is_some() { "available" } else { "unavailable" }, "counters": health},
        "filesystem": filesystem,
        "history": {"status": "unavailable",
            "data": {"schema_version": 0, "cycles_evicted": 0, "queue_dropped": 0,
                "groups_omitted": 0, "previous_history_unavailable": false, "cycles": []}},
        "limitations": [
            "Operation history is not retained by this build. This snapshot describes current local state only; old support history is not read or merged.",
            "Host supervisor, OOM and container restart history are unavailable. Supply those separately for unexplained restarts.",
            "Docker digest is unavailable; provide your image digest separately if needed.",
            "Novel provider fields and historical account participation are unavailable. A maintainer may request a controlled private reproduction.",
            "No media scan, checksum calculation, metadata read or filesystem write probe was performed. Real-platform media verification is case-specific.",
        ],
        "next_actions": next_actions(),
    });
    let bytes = serde_json::to_vec_pretty(&bundle)?;
    if bytes.len() > OUTPUT_BYTES {
        anyhow::bail!("support export exceeds its output limit");
    }
    // Never replace any existing path, including media, state, config or symlinks.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(output).map_err(|_excluded| {
        anyhow::anyhow!("cannot create support export; choose a new writable output filename")
    })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_excluded| {
            anyhow::anyhow!("support export write failed; the output may be incomplete")
        })?;
    Ok(partial)
}

fn next_actions() -> Vec<&'static str> {
    vec![
        "Describe the symptoms, expected behavior, version and incident window. No new sync is required to export.",
        "Attach ordinary service or supervisor logs for the incident window separately. This snapshot does not retain them.",
    ]
}

fn install_method() -> &'static str {
    if Path::new("/.dockerenv").exists() || std::env::var_os("KEI_CONTAINER").is_some() {
        return "docker";
    }
    let executable = std::env::current_exe().unwrap_or_default();
    let name = executable.to_string_lossy();
    if name.contains("/Cellar/") || name.contains("/Homebrew/") {
        "homebrew"
    } else if name.contains("/.cargo/bin/") {
        "cargo"
    } else {
        "unavailable"
    }
}

fn filesystem(raw_directory: Option<&str>) -> Value {
    let Some(raw) = raw_directory else {
        return json!({"status": "unavailable"});
    };
    let directory = crate::config::expand_tilde(raw);
    let entry = std::fs::symlink_metadata(&directory).ok();
    let result = json!({"status": "observational_only", "directory_present": entry.is_some(),
        "directory_is_symlink": entry.is_some_and(|m| m.file_type().is_symlink()),
        "family": "unavailable", "write_semantics": "not_probed"});
    #[cfg(target_os = "linux")]
    let mut result = result;
    #[cfg(target_os = "linux")]
    if let Some(bytes) = read_bytes(Path::new("/proc/self/mountinfo"), 256 * 1024)
        && let Ok(text) = std::str::from_utf8(&bytes)
    {
        let mut longest = 0;
        for line in text.lines() {
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let columns: Vec<_> = left.split_whitespace().collect();
            let Some(mount) = columns.get(4) else {
                continue;
            };
            let mount = mount
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\134", "\\");
            if !directory.starts_with(&mount) || mount.len() < longest {
                continue;
            }
            longest = mount.len();
            let family = match right.split_whitespace().next() {
                Some("ext4" | "ext3" | "ext2") => "ext",
                Some("btrfs") => "btrfs",
                Some("xfs") => "xfs",
                Some("zfs") => "zfs",
                Some("nfs" | "nfs4") => "nfs",
                Some("cifs" | "smb3") => "smb",
                Some("fuse" | "fuseblk" | "fuse.sshfs" | "fuse.rclone") => "fuse",
                Some("overlay") => "overlay",
                Some("tmpfs") => "memory",
                _ => "other",
            };
            if let Some(map) = result.as_object_mut() {
                map.insert("family".into(), json!(family));
                map.insert(
                    "mount_read_only".into(),
                    json!(
                        columns
                            .get(5)
                            .is_some_and(|v| v.split(',').any(|v| v == "ro"))
                    ),
                );
            }
        }
    }
    result
}
