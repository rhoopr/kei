//! Cross-platform helpers shared by service execution and status.
//!
//! `container_supervisor` lets `kei status` report container-managed
//! supervision instead of a misleading `not installed` line. `SERVICE_IDENTIFIER`
//! is the platform service name used by the runtime and status probes.
//! `current_executable` returns a canonical absolute path for service logs
//! so operators can confirm which binary is running.

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

/// Service identifier used by runtime status probes.
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
/// Centralises the one `unsafe` block that wraps `libc::geteuid` for the
/// macOS launchd status probe. POSIX-only: Windows uses access tokens
/// rather than UIDs. The macOS module is compiled on every Unix target
/// for its status parser tests, so this helper stays Unix-wide.
#[cfg(unix)]
pub(crate) fn effective_uid() -> u32 {
    // SAFETY: libc::geteuid is a stateless POSIX FFI call with no
    // memory-safety preconditions, no side effects, and a uid_t return
    // value that cannot violate Rust memory safety.
    unsafe { libc::geteuid() }
}

/// Canonical absolute path to the running `kei` executable.
///
/// The service worker logs an absolute path so operators can confirm which
/// binary a supervisor is running after `PATH` changes or symlinked
/// installs. `std::env::current_exe()` may return a path containing
/// symlinks (e.g. Homebrew's Cellar shim); `canonicalize` resolves those.
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
