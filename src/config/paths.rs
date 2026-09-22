//! Bootstrap, data, credential, and download path resolution.

use super::input::{TomlAuth, TomlConfig};
use crate::types::Domain;
use std::path::{Path, PathBuf};

pub(crate) fn kei_data_dir_with_home(home: &Path) -> PathBuf {
    home.join(".config").join("kei")
}

pub(crate) fn kei_data_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| kei_data_dir_with_home(&home))
        .unwrap_or_else(|| PathBuf::from("~/.config/kei"))
}

pub(crate) fn default_config_path() -> PathBuf {
    kei_data_dir().join("config.toml")
}

pub(crate) fn default_cookie_dir() -> PathBuf {
    kei_data_dir().join("cookies")
}

fn expand_tilde_with_home(path: &str, home: &Path) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        return home.join(stripped);
    }
    PathBuf::from(path)
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with("~/")
        && let Some(home) = dirs::home_dir()
    {
        return expand_tilde_with_home(path, &home);
    }
    PathBuf::from(path)
}

/// Reject system directories that should never be used as a download
/// target. Shared by sync (`Config::build`) and import-existing
/// (`build_import_path_config`) so both refuse the same set with the
/// same error message.
pub(crate) fn validate_download_dir(path: &Path) -> anyhow::Result<()> {
    const DENIED: &[&str] = &[
        "/bin", "/sbin", "/usr", "/etc", "/dev", "/proc", "/sys", "/boot", "/lib", "/lib64",
        "/var", "/root",
    ];
    let s = path.to_string_lossy();
    let trimmed = s.trim_end_matches('/');
    // trimmed.is_empty() catches "/" (trimmed to "")
    if trimmed.is_empty() || DENIED.contains(&trimmed) {
        anyhow::bail!(
            "Refusing to use system directory '{}' as the download directory.",
            path.display()
        );
    }
    Ok(())
}

/// Bootstrap environment values needed by [`super::resolve::resolve_auth`] and `Config::build`.
///
/// These are the narrow env allow-list that remains after v0.20 moved durable
/// settings out of public global CLI flags.
#[derive(Debug, Clone)]
pub(crate) struct GlobalArgs {
    pub username: Option<String>,
    pub domain: Option<Domain>,
    pub data_dir: Option<String>,
}

impl GlobalArgs {
    pub fn from_bootstrap_env() -> Self {
        Self {
            username: std::env::var("ICLOUD_USERNAME").ok(),
            domain: None,
            data_dir: std::env::var("KEI_DATA_DIR").ok(),
        }
    }
}

/// Resolve the data directory (sessions, state DB, credentials, health).
///
/// Resolution order:
/// 1. Explicit `KEI_DATA_DIR` environment variable
/// 2. TOML top-level `data_dir`
/// 3. Default: parent of the resolved config file path
pub(crate) fn resolve_data_dir(
    data_dir_cli: Option<&str>,
    toml: Option<&TomlConfig>,
    config_path: &Path,
) -> PathBuf {
    if let Some(d) = data_dir_cli {
        return expand_tilde(d);
    }
    if let Some(d) = toml.and_then(|t| t.data_dir.as_deref()) {
        return expand_tilde(d);
    }
    config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(kei_data_dir)
}

/// Resolve `password_file` from CLI + TOML.
pub(crate) fn resolve_password_file(
    pw: &crate::cli::PasswordArgs,
    toml_auth: Option<&TomlAuth>,
) -> Option<PathBuf> {
    pw.password_file
        .as_deref()
        .or_else(|| toml_auth.and_then(|a| a.password_file.as_deref()))
        .map(expand_tilde)
}

/// Resolve `password_command` from CLI + TOML.
pub(crate) fn resolve_password_command(
    pw: &crate::cli::PasswordArgs,
    toml_auth: Option<&TomlAuth>,
) -> Option<String> {
    pw.password_command
        .clone()
        .or_else(|| toml_auth.and_then(|a| a.password_command.clone()))
}

#[cfg(test)]
mod tests {
    use super::{
        expand_tilde, expand_tilde_with_home, kei_data_dir_with_home, resolve_data_dir,
        validate_download_dir,
    };
    use crate::config::input::TomlConfig;
    use std::path::{Path, PathBuf};

    #[test]
    fn test_expand_tilde_with_home() {
        let result = expand_tilde("~/Documents");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(result, home.join("Documents"));
        }
    }
    #[test]
    fn test_expand_tilde_with_injected_home_uses_path_join() {
        let home = Path::new("/home/ajlow");
        assert_eq!(
            expand_tilde_with_home("~/.config/kei/cookies", home),
            kei_data_dir_with_home(home).join("cookies")
        );
    }
    #[cfg(windows)]
    #[test]
    fn test_expand_tilde_windows_home_keeps_separator_before_dot_config() {
        let home = Path::new(r"C:\Users\ajlow");
        let result = expand_tilde_with_home("~/.config/kei/cookies", home);
        assert_eq!(result, PathBuf::from(r"C:\Users\ajlow\.config\kei\cookies"));
        assert_ne!(result, PathBuf::from(r"C:\Users\ajlow.config\kei\cookies"));
    }
    #[test]
    fn test_expand_tilde_no_prefix() {
        assert_eq!(
            expand_tilde("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }
    #[test]
    fn test_resolve_data_dir_explicit_cli() {
        let result = resolve_data_dir(Some("/explicit"), None, Path::new("/config/config.toml"));
        assert_eq!(result, PathBuf::from("/explicit"));
    }
    #[test]
    fn test_resolve_data_dir_toml_data_dir() {
        let toml = TomlConfig {
            data_dir: Some("/toml/data".to_string()),
            log_level: None,
            auth: None,
            download: None,
            filters: None,
            photos: None,
            import: None,
            metadata: None,
            watch: None,
            notifications: None,
            server: None,
            report: None,
            ui: None,
        };
        let result = resolve_data_dir(None, Some(&toml), Path::new("/config/config.toml"));
        assert_eq!(result, PathBuf::from("/toml/data"));
    }
    #[test]
    fn test_resolve_data_dir_defaults_to_config_parent() {
        let result = resolve_data_dir(None, None, Path::new("/config/config.toml"));
        assert_eq!(result, PathBuf::from("/config"));
    }
    #[test]
    fn test_resolve_data_dir_cli_takes_precedence_over_toml() {
        let toml = TomlConfig {
            data_dir: Some("/toml/data".to_string()),
            log_level: None,
            auth: None,
            download: None,
            filters: None,
            photos: None,
            import: None,
            metadata: None,
            watch: None,
            notifications: None,
            server: None,
            report: None,
            ui: None,
        };
        let result = resolve_data_dir(
            Some("/cli/data"),
            Some(&toml),
            Path::new("/config/config.toml"),
        );
        assert_eq!(result, PathBuf::from("/cli/data"));
    }
    #[test]
    fn test_validate_download_dir_rejects_root() {
        assert!(validate_download_dir(Path::new("/")).is_err());
    }
    #[test]
    fn test_validate_download_dir_rejects_system_paths() {
        for path in ["/usr", "/etc", "/boot", "/sys", "/proc", "/dev", "/var"] {
            assert!(
                validate_download_dir(Path::new(path)).is_err(),
                "should reject {path}"
            );
        }
    }
    #[test]
    fn test_validate_download_dir_rejects_trailing_slash() {
        assert!(validate_download_dir(Path::new("/etc/")).is_err());
    }
    #[test]
    fn test_validate_download_dir_accepts_normal_paths() {
        assert!(validate_download_dir(Path::new("/home/user/photos")).is_ok());
        assert!(validate_download_dir(Path::new("/mnt/photos")).is_ok());
        assert!(validate_download_dir(Path::new("/data/sync")).is_ok());
    }
}
