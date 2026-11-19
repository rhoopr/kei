//! `kei uninstall` dispatcher.
//!
//! Symmetric to [`crate::service::install`]: removes the per-platform
//! service registration and, with `--purge`, the state database,
//! configuration, and stored credentials. Container short-circuit
//! matches install — compose-managed deployments aren't touched.

use anyhow::Result;
use std::path::Path;

use crate::cli::UninstallArgs;
use crate::config::{self, GlobalArgs, TomlConfig};

pub(crate) async fn run(
    args: UninstallArgs,
    config_path: &Path,
    globals: &GlobalArgs,
    toml: Option<&TomlConfig>,
) -> Result<()> {
    if crate::service::env::container_supervisor().is_some() {
        tracing::info!(
            "kei uninstall is a no-op inside containers; \
             stop and remove the docker-compose stack instead"
        );
        return Ok(());
    }

    // Never translate --config or data_dir into permission to recursively
    // delete an arbitrary parent (for example /etc or the user's home).
    let root = if args.purge {
        let root = config::kei_data_dir()?;
        let data = config::resolve_data_dir(
            globals.data_dir.as_deref(),
            toml.and_then(|t| t.data_dir.as_deref()),
        )?;
        validate_purge_paths(&root, config_path, &data)?;
        Some(root)
    } else {
        None
    };
    dispatch(root.as_deref()).await
}

fn validate_purge_paths(root: &Path, config: &Path, data: &Path) -> Result<()> {
    anyhow::ensure!(
        config == root.join("config.toml") && (data == root || data == root.join("cookies")),
        "Refusing to purge with custom config or state paths. Uninstall without --purge, then inspect the paths before removing kei files manually."
    );
    anyhow::ensure!(
        !root.is_symlink(),
        "Refusing to purge a symlinked kei root at {}. Inspect its target and remove kei files manually.",
        root.display()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
async fn dispatch(root: Option<&Path>) -> Result<()> {
    crate::service::linux::uninstall(root).await
}

#[cfg(target_os = "macos")]
async fn dispatch(root: Option<&Path>) -> Result<()> {
    crate::service::macos::uninstall(root).await
}

#[cfg(target_os = "windows")]
async fn dispatch(root: Option<&Path>) -> Result<()> {
    crate::service::windows::uninstall(root).await
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn dispatch(root: Option<&Path>) -> Result<()> {
    use crate::service::env::SERVICE_IDENTIFIER;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        purge = root.is_some(),
        "preparing to uninstall kei service",
    );
    Err(anyhow::anyhow!(
        "`kei uninstall` is not available on this platform."
    ))
}

#[cfg(test)]
mod tests {
    use super::validate_purge_paths;

    #[test]
    fn purge_accepts_only_the_selected_installation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("kei");
        let config = root.join("config.toml");
        let cookies = root.join("cookies");
        assert!(validate_purge_paths(&root, &config, &cookies).is_ok());
        assert!(validate_purge_paths(&root, &config, &root).is_ok());
        assert!(validate_purge_paths(&root, &config, dir.path()).is_err());
        assert!(validate_purge_paths(&root, &dir.path().join("other.toml"), &cookies).is_err());
    }
}
