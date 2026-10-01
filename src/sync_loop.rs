//! Sync command facade. Keeps command-facing paths stable.
//!
//! Ownership: session authentication and recovery (`session`), scoped provider
//! prechecks (`precheck`), library plans and notices (`planning`), local drift
//! (`reconcile`), watch policy (`watch`), and cycle reporting (`reporting`).
//! `runner` composes these owners without changing their safety gates.

use std::sync::Arc;

use crate::password::SecretString;
use crate::{cli, config};

mod planning;
mod precheck;
mod reconcile;
mod reporting;
mod runner;
mod session;
mod watch;

#[cfg(test)]
mod test_support;

pub(crate) use planning::count_passes;
pub(crate) use reconcile::should_reconcile_this_cycle;
pub(crate) use runner::run_sync;
pub(crate) use session::should_wait_for_2fa;
pub(crate) use watch::{SERVICE_MODE_DEFAULT_WATCH_INTERVAL, service_mode_default_interval};

/// Arguments that [`run_sync`] needs from the CLI dispatch layer.
#[derive(Clone)]
pub(crate) struct SyncArgs {
    pub is_one_shot: bool,
    /// True when invoked via `kei service run`. After [`Config::build`]
    /// resolves CLI > TOML > env, a still-unset watch interval falls
    /// through to [`SERVICE_MODE_DEFAULT_WATCH_INTERVAL`] so the daemon
    /// always polls (single-shot service-mode is meaningless).
    pub service_mode: bool,
    pub pw: cli::PasswordArgs,
    pub sync: cli::SyncArgs,
    pub toml_config: Option<config::TomlConfig>,
    pub config_explicitly_set: bool,
    pub config_path: std::path::PathBuf,
    pub redact_password: Arc<std::sync::Mutex<Option<SecretString>>>,
    /// Resolved friendly UX mode from lib.rs startup. Threaded into Config so
    /// the download pipeline picks the right bar template.
    pub personality_mode: crate::personality::Mode,
    /// User-stated friendly preference (CLI > TOML; `None` means neither was
    /// set, so the default-on-for-TTY policy is active). Threaded so the
    /// resolved Config exposes the same intent the gate saw, useful for
    /// downstream code that wants to know whether the user opted in.
    pub friendly_request: Option<bool>,
    /// Whether startup detected a terminal on stdin.
    pub input_mode: crate::InputMode,
}
