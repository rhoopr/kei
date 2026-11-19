//! Bootstrap, data, credential, and download path resolution.

use super::input::TomlAuth;
use crate::types::Domain;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

fn kei_data_dir_with_home(home: &Path) -> PathBuf {
    home.join(".config").join("kei")
}

pub(crate) fn kei_data_dir() -> Result<PathBuf> {
    let legacy = dirs::home_dir().map(|home| kei_data_dir_with_home(&home));
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    discover_root(legacy, xdg.as_deref())
}

fn discover_root(legacy: Option<PathBuf>, xdg: Option<&Path>) -> Result<PathBuf> {
    let Some(xdg) = xdg.filter(|p| p.is_absolute()).map(|p| p.join("kei")) else {
        return legacy.context("Cannot locate kei's root: set HOME or an absolute XDG_CONFIG_HOME");
    };
    let Some(legacy) = legacy.filter(|p| *p != xdg) else {
        return Ok(xdg);
    };
    // A dangling link or inaccessible directory isn't evidence of a fresh install.
    let existing = |p: &Path| -> Result<bool> {
        match std::fs::symlink_metadata(p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Ok(entry) if entry.is_dir() || (entry.is_symlink() && p.is_dir()) => Ok(true),
            Ok(_) => bail!("Expected a readable kei directory at {}", p.display()),
            Err(e) => Err(e).with_context(|| format!("Cannot inspect {}", p.display())),
        }
    };
    match (existing(&legacy)?, existing(&xdg)?) {
        (false, _) => Ok(xdg),
        (true, false) => Ok(legacy),
        (true, true) if legacy.canonicalize()? == xdg.canonicalize()? => Ok(xdg),
        (true, true) => bail!(
            "Found two kei roots: {} and {}. Inspect both directories before moving either. To run without automatic discovery, select your config with --config and state with KEI_DATA_DIR or data_dir. Purge requires one unambiguous root.",
            legacy.display(),
            xdg.display()
        ),
    }
}

pub(crate) fn bootstrap_config(explicit: Option<&str>) -> Result<(PathBuf, bool)> {
    if let Some(path) = explicit {
        return Ok((expand_tilde(path), false));
    }
    let root = kei_data_dir()?;
    let path = root.join("config.toml");
    let docker = Path::new("/config/config.toml");
    if !path.try_exists()? && docker.is_file() {
        return Ok((docker.to_owned(), true));
    }
    Ok((path, false))
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
    /// Captured with startup parsing, never regenerated from resolved settings.
    pub config_source: super::ConfigSource,
    pub username: Option<String>,
    pub domain: Option<Domain>,
    pub data_dir: Option<String>,
}

impl GlobalArgs {
    pub fn from_bootstrap_env() -> Self {
        Self {
            config_source: super::ConfigSource::Unavailable,
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
/// 3. Default: cookies under the discovered application root
pub(crate) fn resolve_data_dir(
    data_dir_cli: Option<&str>,
    configured: Option<&str>,
) -> Result<PathBuf> {
    if let Some(d) = data_dir_cli.or(configured) {
        return Ok(expand_tilde(d));
    }
    Ok(kei_data_dir()?.join("cookies"))
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
    use std::path::{Path, PathBuf};

    #[test]
    fn relative_or_empty_xdg_keeps_dotted_config_root() {
        let legacy = kei_data_dir_with_home(&std::env::temp_dir());
        for xdg in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            assert_eq!(
                super::discover_root(Some(legacy.clone()), xdg).unwrap(),
                legacy
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn dangling_legacy_root_is_not_a_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        std::os::unix::fs::symlink(dir.path().join("offline"), &legacy).unwrap();
        let xdg = dir.path().join("new");
        let error = super::discover_root(Some(legacy.clone()), Some(&xdg)).unwrap_err();
        assert!(error.to_string().contains(legacy.to_str().unwrap()));
        assert!(!xdg.exists());
    }

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
        let result = resolve_data_dir(Some("/explicit"), None).unwrap();
        assert_eq!(result, PathBuf::from("/explicit"));
    }
    #[test]
    fn test_resolve_data_dir_toml_data_dir() {
        let result = resolve_data_dir(None, Some("/toml/data")).unwrap();
        assert_eq!(result, PathBuf::from("/toml/data"));
    }
    #[test]
    fn test_resolve_data_dir_defaults_to_cookie_directory() {
        let result = resolve_data_dir(None, None).unwrap();
        assert_eq!(result, super::kei_data_dir().unwrap().join("cookies"));
    }
    #[test]
    fn test_resolve_data_dir_cli_takes_precedence_over_toml() {
        let result = resolve_data_dir(Some("/cli/data"), Some("/toml/data")).unwrap();
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
