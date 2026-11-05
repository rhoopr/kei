//! Service-mode helpers consumed by `kei service run` and
//! `kei service status`.
//!
//! Cross-platform plumbing (container detection, branding constants,
//! executable canonicalization) lives in `env`. The execution and status
//! dispatchers route through `cfg(target_os = ...)` to per-platform
//! backends: linux, macos, or windows.

pub(crate) mod env;
pub(crate) mod run;
pub(crate) mod status;

#[cfg(target_os = "linux")]
pub(crate) mod linux;

// Compiled on every unix target (linux + macOS) because the status parser
// and formatter carry inline unit tests that run on linux CI before
// macos-latest sees them. Windows is excluded because `effective_uid`
// (POSIX `geteuid`) and `tokio::process::Command` calls to `launchctl`
// have no analogue there. Only the `kei service status` dispatcher is
// runtime-gated to macOS -- see status.rs.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod macos;

// Compiled on every target -- pure status formatter helpers carry inline
// unit tests that run on linux + macOS, and only the windows-service FFI
// surface is `#[cfg(target_os = "windows")]`-gated inside the module. As
// with macos, the status and run dispatchers runtime-gate to Windows
// targets.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod windows;
