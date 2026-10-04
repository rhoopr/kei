//! Cross-platform helpers shared by every service-install backend.
//!
//! `container_supervisor` lets `kei install` no-op cleanly inside Docker so
//! containerized deployments keep using the existing compose workflow
//! instead of attempting to register a launchd/systemd/SCM service that
//! the host would never invoke. `service_identifier` and
//! `SERVICE_DESCRIPTION` are the branding constants every backend writes
//! into its registry entry. `current_executable` returns a canonical
//! absolute path so service files survive PATH changes, working-directory
//! shifts (Windows SCM starts services in `C:\Windows\System32`), and
//! symlinked installs (Homebrew Cellar -> /usr/local/bin).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Marker file Docker creates inside every container.
const DEFAULT_DOCKERENV_PATH: &str = "/.dockerenv";

const DEFAULT_CGROUP_PATH: &str = "/proc/1/cgroup";

/// Substrings in `/proc/1/cgroup` that signal a container runtime.
/// Covers Docker, containerd (Kubernetes worker default), Kubernetes
/// pod cgroup hierarchy, Podman, and LXC. Only consulted on Linux.
#[cfg(target_os = "linux")]
const CONTAINER_CGROUP_MARKERS: &[&str] = &["docker", "containerd", "kubepods", "podman", "lxc"];

/// Human-readable description shown in service-manager registries.
pub(crate) const SERVICE_DESCRIPTION: &str = "kei Media Sync Engine";

/// Service identifier used by the per-platform installer.
///
/// Reverse-DNS form on macOS (launchd `Label`) and Windows (SCM service
/// name); plain `kei` on Linux (systemd unit file basename).
pub(crate) const SERVICE_IDENTIFIER: &str = if cfg!(target_os = "linux") {
    "kei"
} else {
    "com.rhoopr.kei"
};

/// Returns the name of the container supervisor when running inside a
/// container, or `None` on the bare host.
///
/// Used by `kei status` to render the service as container-managed instead
/// of `not installed`, which would mislead operators on a Docker /
/// Kubernetes host where service management is the host's job.
/// Two-signal probe: the marker file Docker drops at `/.dockerenv`
/// (always reads as "docker"), then the cgroup membership of PID 1
/// (returns the matched marker). Cgroup
/// parsing is Linux-only; on macOS/Windows only the marker-file check
/// runs.
pub(crate) fn container_supervisor() -> Option<&'static str> {
    container_supervisor_at(
        Path::new(DEFAULT_DOCKERENV_PATH),
        Path::new(DEFAULT_CGROUP_PATH),
    )
}

/// Path-injecting form of [`container_supervisor`].
pub(crate) fn container_supervisor_at(dockerenv: &Path, cgroup: &Path) -> Option<&'static str> {
    if dockerenv.exists() {
        return Some("docker");
    }

    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(cgroup) {
            Ok(contents) => CONTAINER_CGROUP_MARKERS
                .iter()
                .find(|marker| contents.contains(*marker))
                .copied(),
            Err(_) => None,
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cgroup;
        None
    }
}

/// Effective UID of the current process.
///
/// Centralises the one `unsafe` block per backend that wraps
/// `libc::geteuid` so each platform module doesn't carry its own copy
/// with a duplicated SAFETY comment. POSIX-only -- Windows uses access
/// tokens rather than UIDs, and the linux + macOS service backends are
/// the only callers.
#[cfg(unix)]
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: libc::geteuid is a stateless POSIX FFI call with no
    // memory-safety preconditions, no side effects, and a uid_t return
    // value that cannot violate Rust memory safety.
    unsafe { libc::geteuid() }
}

/// Reads `auth.username` out of `kei_dir/config.toml`.
///
/// Returns `None` when the file is missing, malformed, or has no
/// non-empty username -- `--purge` callers treat any of those as "no
/// credential to clear" and proceed. Cross-platform: Linux + macOS +
/// Windows backends all share this helper.
pub(crate) fn read_config_username(kei_dir: &Path) -> Option<String> {
    let config_path = kei_dir.join("config.toml");
    let toml = crate::config::load_toml_config(&config_path, false).ok()??;
    toml.auth?.username.filter(|u| !u.is_empty())
}

/// Wipes `kei_dir` plus any platform-specific extras after clearing the
/// stored credential. Each platform's `--purge` path resolves the kei
/// state directory however it wants (linux honours `XDG_CONFIG_HOME`;
/// macOS and Windows hard-code `~/.config/kei` rather than the platform
/// default config dir) and passes the resolved path here.
pub(crate) fn purge_kei_state(kei_dir: &Path, extra_dirs: &[PathBuf]) -> Result<()> {
    if let Some(username) = read_config_username(kei_dir) {
        let realm = crate::config::load_toml_config(&kei_dir.join("config.toml"), false)
            .ok()
            .flatten()
            .and_then(|config| config.auth)
            .and_then(|auth| auth.domain)
            .unwrap_or(crate::types::Domain::Com);
        let store = crate::credential::CredentialStore::new(&username, kei_dir, realm.as_str());
        match store.delete() {
            Err(e) => {
                // delete() bails when neither backend has anything to remove,
                // which is fine for purge - we're cleaning up regardless.
                tracing::debug!(error = %e, "credential delete during purge: nothing to remove");
            }
            Ok(()) => {
                tracing::info!(username, "cleared stored credential");
            }
        }
    }

    for dir in extra_dirs {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => tracing::info!(path = %dir.display(), "removed auxiliary purge directory"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                error = %e,
                path = %dir.display(),
                "failed to remove auxiliary purge directory",
            ),
        }
    }

    match std::fs::remove_dir_all(kei_dir) {
        Ok(()) => {
            tracing::info!(path = %kei_dir.display(), "purged kei state directory");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %kei_dir.display(), "no kei state directory to purge");
            Ok(())
        }
        Err(e) => Err(e)
            .with_context(|| format!("Could not remove state directory {}", kei_dir.display())),
    }
}

/// kei state directory at the dotted-config convention.
///
/// macOS and Windows both bypass `dirs::config_dir()` (which returns
/// `~/Library/Application Support` and `%APPDATA%\Roaming` respectively)
/// so service purge paths match the rest of kei's `~/.config/kei` state.
pub(crate) fn kei_state_dir_dotted() -> Option<PathBuf> {
    dirs::home_dir().map(|home| crate::config::kei_data_dir_with_home(&home))
}

/// Canonical absolute path to the running `kei` executable.
///
/// Service unit files / launchd plists / SCM entries reference an
/// absolute path so they keep working after `PATH` changes or
/// working-directory shifts. `std::env::current_exe()` may return a
/// path containing symlinks (e.g. Homebrew's Cellar shim);
/// `canonicalize` resolves those so the registry entry points at the
/// real binary.
pub(crate) fn current_executable() -> Result<PathBuf> {
    let raw = std::env::current_exe().context("Could not find the current executable path")?;
    raw.canonicalize()
        .with_context(|| format!("Could not resolve executable path {}", raw.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn service_identifier_matches_platform_branding() {
        if cfg!(target_os = "linux") {
            assert_eq!(SERVICE_IDENTIFIER, "kei");
        } else {
            assert_eq!(SERVICE_IDENTIFIER, "com.rhoopr.kei");
        }
    }

    #[test]
    fn service_description_is_pinned() {
        // Phase-1 plan locked this string. Regression-guard so a casual
        // rename doesn't slip past review and orphan every installed
        // service entry on user machines.
        assert_eq!(SERVICE_DESCRIPTION, "kei Media Sync Engine");
    }

    #[test]
    fn dockerenv_marker_short_circuits_cgroup_check() {
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv");
        fs::write(&dockerenv, "").unwrap();
        let cgroup = tmp.path().join("cgroup");
        fs::write(&cgroup, "").unwrap();
        assert!(container_supervisor_at(&dockerenv, &cgroup).is_some());
    }

    #[test]
    fn dockerenv_absent_and_cgroup_clean_returns_false() {
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv-missing");
        let cgroup = tmp.path().join("cgroup");
        fs::write(&cgroup, "0::/user.slice/user-1000.slice\n").unwrap();
        assert!(container_supervisor_at(&dockerenv, &cgroup).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_with_container_marker_returns_true() {
        let cases: &[(&str, &str)] = &[
            (
                "docker",
                "0::/docker/abc123def456\n12:cpu:/docker/abc123def456\n",
            ),
            ("containerd", "0::/system.slice/containerd.service\n"),
            (
                "kubepods",
                "0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-podabc.slice\n",
            ),
            (
                "podman",
                "0::/user.slice/user-1000.slice/user@1000.service/podman-1234.scope\n",
            ),
            ("lxc", "0::/lxc/container-name\n"),
        ];

        for (name, contents) in cases {
            let tmp = TempDir::new().unwrap();
            let dockerenv = tmp.path().join("dockerenv-missing");
            let cgroup = tmp.path().join("cgroup");
            fs::write(&cgroup, contents).unwrap();
            assert_eq!(
                container_supervisor_at(&dockerenv, &cgroup),
                Some(*name),
                "expected supervisor {name} from cgroup contents",
            );
        }
    }

    #[test]
    fn container_supervisor_dockerenv_marker_returns_docker() {
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv");
        fs::write(&dockerenv, "").unwrap();
        let cgroup = tmp.path().join("cgroup");
        fs::write(&cgroup, "").unwrap();
        assert_eq!(container_supervisor_at(&dockerenv, &cgroup), Some("docker"),);
    }

    #[test]
    fn container_supervisor_returns_none_on_clean_host() {
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv-missing");
        let cgroup = tmp.path().join("cgroup");
        fs::write(&cgroup, "0::/user.slice/user-1000.slice\n").unwrap();
        assert_eq!(container_supervisor_at(&dockerenv, &cgroup), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_cgroup_file_is_not_a_container() {
        // Bare-metal hosts without /proc/1/cgroup (some sandboxes, chroots)
        // must answer "not in a container" rather than propagate an error.
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv-missing");
        let cgroup = tmp.path().join("cgroup-does-not-exist");
        assert!(container_supervisor_at(&dockerenv, &cgroup).is_none());
    }

    #[test]
    fn current_executable_returns_existing_absolute_path() {
        let exe = current_executable().unwrap();
        assert!(
            exe.is_absolute(),
            "expected absolute path, got {}",
            exe.display()
        );
        assert!(
            exe.exists(),
            "executable must exist on disk: {}",
            exe.display()
        );
    }
}
