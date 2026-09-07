//! Sync loop: the watch-mode cycle that enumerates and downloads photos.
//!
//! Extracted from `main.rs` to keep the entry point focused on CLI dispatch.
//! The public entry point is [`run_sync`], which handles config resolution,
//! authentication, the download loop, and watch-mode re-sync.

use std::sync::Arc;

use anyhow::Context;

use crate::auth;
use crate::cli;
#[cfg(test)]
use crate::commands::PassScope;
use crate::commands::{
    CollectionContext, MAX_REAUTH_ATTEMPTS, attempt_reauth, build_collection_context,
    collection_libraries, init_photos_service, pass_scope_for_zone,
    resolve_cross_zone_libraries_for_album_hydration, resolve_libraries, resolve_passes_for_scope,
    wait_and_retry_2fa, zone_name_set,
};
use crate::config;
use crate::credential;
use crate::download;
use crate::health;
use crate::notifications::{self, Notifier};
use crate::password::{self, ExposeSecret, SecretString};
use crate::retry;
use crate::shutdown;
use crate::state;
use crate::sync_cycle::{LibraryState, run_cycle, sync_token_key as make_sync_token_key};

#[cfg(test)]
use crate::sync_cycle::{
    CycleResult, DownloadConfigHashOutcome, ENUM_CONFIG_HASH_KEY, EnumConfigHashOutcome,
    PENDING_DOWNLOAD_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, SYNC_TOKEN_PREFIX,
    check_and_persist_enum_config_hash, check_download_config_hash_for_cycle, determine_sync_mode,
    pending_zone_token_key, should_store_sync_token, should_store_sync_token_for_cycle,
};

#[cfg(all(test, feature = "xmp"))]
use crate::sync_cycle::preload_asset_groupings;

use crate::systemd::SystemdNotifier;
use crate::{
    PartialSyncError, PidFileGuard, available_disk_space, check_min_disk_space,
    make_password_provider,
};
#[cfg(test)]
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
enum WatchPrecheck {
    SkipAll,
    Proceed {
        changed_zones: Option<rustc_hash::FxHashSet<String>>,
        db_sync_token_after_success: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncAuthErrorClass {
    TwoFactorRequired,
    LockContention,
    SessionReset,
    Other,
}

/// Classify auth errors at the sync-loop orchestration boundary.
///
/// The sync loop uses these classes for distinct behaviors: initial-auth 2FA
/// wait, mid-cycle reauth 2FA wait vs one-shot return, and lock-reacquire
/// shutdown messaging. Callers still return the original `anyhow::Error`.
fn classify_sync_auth_error(err: &anyhow::Error) -> SyncAuthErrorClass {
    let Some(auth_err) = err.downcast_ref::<auth::error::AuthError>() else {
        return SyncAuthErrorClass::Other;
    };
    if auth_err.is_two_factor_required() {
        SyncAuthErrorClass::TwoFactorRequired
    } else if auth_err.is_lock_contention() {
        SyncAuthErrorClass::LockContention
    } else if auth_err.is_session_reset() {
        SyncAuthErrorClass::SessionReset
    } else {
        SyncAuthErrorClass::Other
    }
}

fn two_factor_recovery_message(username: &str) -> String {
    format!(
        "2FA required for {username}. Run `kei login get-code`, then `kei login submit-code <CODE>`."
    )
}

impl WatchPrecheck {
    fn proceed_all() -> Self {
        Self::Proceed {
            changed_zones: None,
            db_sync_token_after_success: None,
        }
    }

    fn changed_zones(&self) -> Option<&rustc_hash::FxHashSet<String>> {
        match self {
            Self::SkipAll => None,
            Self::Proceed { changed_zones, .. } => changed_zones.as_ref(),
        }
    }

    fn db_sync_token_after_success(&self) -> Option<&str> {
        match self {
            Self::SkipAll => None,
            Self::Proceed {
                db_sync_token_after_success,
                ..
            } => db_sync_token_after_success.as_deref(),
        }
    }

    fn should_sync_zone(&self, zone_name: &str) -> bool {
        match self {
            Self::SkipAll => false,
            Self::Proceed {
                changed_zones: Some(zones),
                ..
            } => zones.contains(zone_name),
            Self::Proceed {
                changed_zones: None,
                ..
            } => true,
        }
    }

    fn include_local_work_zones(&mut self, zones: rustc_hash::FxHashSet<String>) {
        if zones.is_empty() {
            return;
        }
        match self {
            Self::SkipAll => {
                *self = Self::Proceed {
                    changed_zones: Some(zones),
                    db_sync_token_after_success: None,
                };
            }
            Self::Proceed {
                changed_zones: Some(changed_zones),
                ..
            } => changed_zones.extend(zones),
            Self::Proceed {
                changed_zones: None,
                ..
            } => {}
        }
    }
}

async fn include_pending_metadata_work(
    watch_precheck: &mut WatchPrecheck,
    db: &dyn download::DownloadStore,
    metadata: &config::MetadataConfig,
    library_states: &[LibraryState],
) {
    let rewrite_writers_enabled = download::metadata_rewrite::writers_enabled(metadata);
    let mut local_work_zones = rustc_hash::FxHashSet::default();
    for library in library_states {
        let zone = library.zone_name.as_str();
        let capture_pending = match db
            .has_metadata_capture_work(&[zone], state::METADATA_CAPTURE_REVISION)
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(error = %error, "Could not inspect metadata-capture work before watch pre-check");
                true
            }
        };
        let rewrite_pending = if rewrite_writers_enabled {
            match db
                .get_pending_metadata_rewrites_page(Some(&[zone]), 0, 1)
                .await
            {
                Ok(rows) => !rows.is_empty(),
                Err(error) => {
                    tracing::warn!(error = %error, "Could not inspect metadata-rewrite work before watch pre-check");
                    true
                }
            }
        } else {
            false
        };
        if capture_pending || rewrite_pending {
            local_work_zones.insert(library.zone_name.clone());
        }
    }
    if !local_work_zones.is_empty() {
        tracing::debug!(
            libraries = local_work_zones.len(),
            "Pending metadata work bypassed the watch no-change shortcut"
        );
        watch_precheck.include_local_work_zones(local_work_zones);
    }
}

/// State-DB metadata key for the first-sync shared-library notice. Bumping
/// the version suffix (e.g. `_v2`) re-fires the notice for every existing
/// data dir the next time it's used.
const SHARED_LIBRARY_NOTICE_KEY: &str = "shared_library_notice_shown_v1";
const SHARED_LIBRARY_NOTICE_CHECKED_KEY: &str = "shared_library_notice_checked_at_v1";
const SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS: i64 = 24 * 60 * 60;

/// Legacy metadata key for the unscoped database-level token used by
/// `/changes/database` before scoped provenance rows.
#[cfg(test)]
const DB_SYNC_TOKEN_KEY: &str = "db_sync_token";
const SCOPED_DB_SYNC_TOKEN_PROVIDER: &str = "icloud";
const SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DbPrecheckScope {
    provider: String,
    account: String,
    shape_version: i64,
    scope_hash: String,
    selected_zones_json: String,
    scope_json: String,
}

impl DbPrecheckScope {
    fn from_config(
        config: &config::Config,
        library_states: &[LibraryState],
        build_download_config: &crate::sync_cycle::BuildDownloadConfigFn<'_>,
        enum_config_hash: &str,
    ) -> anyhow::Result<Self> {
        let mut selected_zones: Vec<String> =
            library_states.iter().map(|s| s.zone_name.clone()).collect();
        selected_zones.sort();

        let download_config_hash = build_download_config(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from(
                selected_zones
                    .first()
                    .map(String::as_str)
                    .unwrap_or(crate::icloud::photos::PRIMARY_ZONE_NAME),
            ),
        );
        let download_config_hash = download::hash_download_config(&download_config_hash);

        let selected_zones_json = serde_json::to_string(&selected_zones)
            .context("serialize scoped database token selected zones")?;
        let scope_json = download::sync_coverage_fingerprint_json(
            config,
            SCOPED_DB_SYNC_TOKEN_PROVIDER,
            SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION,
            &selected_zones,
            enum_config_hash,
            &download_config_hash,
        )?;
        let scope_hash =
            hash_scoped_db_precheck_scope(SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION, &scope_json);

        Ok(Self {
            provider: SCOPED_DB_SYNC_TOKEN_PROVIDER.to_string(),
            account: config.auth.username.clone(),
            shape_version: SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION,
            scope_hash,
            selected_zones_json,
            scope_json,
        })
    }

    fn to_state_row(&self, token: &str) -> state::ScopedDbSyncToken {
        state::ScopedDbSyncToken {
            provider: self.provider.clone(),
            account: self.account.clone(),
            shape_version: self.shape_version,
            scope_hash: self.scope_hash.clone(),
            selected_zones_json: self.selected_zones_json.clone(),
            scope_json: self.scope_json.clone(),
            token: token.to_string(),
        }
    }
}

fn hash_scoped_db_precheck_scope(shape_version: i64, scope_json: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let mut hasher = Sha256::new();
    hasher.update(shape_version.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(scope_json.as_bytes());
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in hash {
        let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

/// Classify whether an error from `init_photos_service` or
/// `resolve_libraries` indicates a stale session / routing state that
/// an SRP re-auth would fix.
///
/// Returns `true` for `ICloudError::SessionExpired` (CloudKit 401/403)
/// and `ICloudError::MisdirectedRequest` (persistent 421 after pool
/// reset) — the two classes that invalidate the cached session and
/// trigger the reauth retry branch. Extracted as a free function so
/// the classification is independently testable without spinning up
/// a full sync cycle.
fn is_session_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<crate::icloud::error::ICloudError>()
        .is_some_and(crate::icloud::error::ICloudError::is_session_error)
}

/// Whether a CloudKit init/query error should trigger an SRP re-auth retry.
///
/// Returns `true` only on the first session-error encounter. A second
/// session-error bails cleanly instead of looping under Docker's restart
/// policy.
fn should_retry_session_init(err: &anyhow::Error, already_retried: bool) -> bool {
    !already_retried && is_session_error(err)
}

fn take_pending_auth<T>(pending_auth: &mut Option<T>) -> anyhow::Result<T> {
    pending_auth
        .take()
        .ok_or_else(|| anyhow::anyhow!("internal auth retry state missing before attempt"))
}

/// Given the user's library selector, the count of iCloud shared libraries
/// on the account, and whether the notice has already fired, return the
/// warning message to emit, or `None` if no notice is warranted.
///
/// Pure function so the policy is unit-testable without mocking the
/// `PhotosService` or the state DB. The I/O wrapper lives in
/// [`maybe_notify_shared_libraries`].
fn should_notify_shared_libraries(
    selector: &crate::selection::LibrarySelector,
    shared_count: usize,
    already_notified: bool,
) -> Option<String> {
    if already_notified || shared_count == 0 {
        return None;
    }
    // Only users on the `primary`-only default see the notice. Anyone who
    // explicitly picked a different shape (`shared`, `all`, named zones,
    // exclusions) has already made a deliberate choice.
    if selector != &crate::selection::LibrarySelector::default() {
        return None;
    }
    let (word, verb) = if shared_count == 1 {
        ("library", "is")
    } else {
        ("libraries", "are")
    };
    Some(format!(
        "Detected {shared_count} iCloud shared {word} on this account; only the primary \
         library {verb} being synced. To include shared libraries too, set \
         `[filters] libraries = [\"all\"]` in config.toml. \
         Run `kei list libraries` to enumerate every zone."
    ))
}

fn shared_library_notice_recently_checked(checked_at: Option<&str>, now_ts: i64) -> bool {
    let Some(checked_at) = checked_at.and_then(|value| value.parse::<i64>().ok()) else {
        return false;
    };
    now_ts.saturating_sub(checked_at) < SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS
}

/// Probe + warning for users on the `PrimarySync` default who also have
/// shared libraries. The notice marker (stored in the state DB's `metadata`
/// table under [`SHARED_LIBRARY_NOTICE_KEY`]) is set after the notice fires.
/// A separate short-lived negative cache records "no shared libraries seen"
/// so accounts without shared libraries do not pay a shared-zone listing on
/// every one-shot sync. The probe and marker writes are best-effort: failures
/// degrade to `tracing::debug!` and skip without breaking the sync.
async fn maybe_notify_shared_libraries(
    selector: &crate::selection::LibrarySelector,
    photos_service: &mut crate::icloud::photos::PhotosService,
    state_db: Option<&dyn state::SyncTokenStore>,
) {
    let Some(db) = state_db else {
        tracing::debug!("shared-library notice: no state DB available; skipping uncached probe");
        return;
    };

    let already_notified = match db.get_metadata(SHARED_LIBRARY_NOTICE_KEY).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: metadata read failed; skipping"
            );
            return;
        }
    };
    if already_notified {
        return;
    }

    // Skip the probe when the user explicitly picked a non-default library;
    // they've already opted in or out. `should_notify_shared_libraries`
    // repeats this check defensively.
    if selector != &crate::selection::LibrarySelector::default() {
        return;
    }

    match db.get_metadata(SHARED_LIBRARY_NOTICE_CHECKED_KEY).await {
        Ok(checked_at) => {
            if shared_library_notice_recently_checked(
                checked_at.as_deref(),
                chrono::Utc::now().timestamp(),
            ) {
                tracing::debug!("shared-library notice: recent no-shared check cached");
                return;
            }
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: checked-at read failed; probing"
            );
        }
    }

    let shared_count = match photos_service.fetch_shared_libraries().await {
        Ok(map) => map.len(),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: enumeration failed; skipping"
            );
            return;
        }
    };

    let Some(msg) = should_notify_shared_libraries(selector, shared_count, already_notified) else {
        if shared_count == 0
            && let Err(e) = db
                .set_metadata(
                    SHARED_LIBRARY_NOTICE_CHECKED_KEY,
                    &chrono::Utc::now().timestamp().to_string(),
                )
                .await
        {
            tracing::debug!(
                error = %e,
                "shared-library notice: failed to persist no-shared check marker"
            );
        }
        return;
    };
    tracing::warn!(message = %msg, "Shared library notice");

    if let Err(e) = db.set_metadata(SHARED_LIBRARY_NOTICE_KEY, "1").await {
        tracing::debug!(
            error = %e,
            "shared-library notice: failed to persist marker"
        );
    }
}

/// Default watch interval applied when `kei service run` enters with no TOML
/// value set. 24 hours, matching the Docker image's always-on service shape.
pub(crate) const SERVICE_MODE_DEFAULT_WATCH_INTERVAL: u64 = 86400;

/// Maximum idle delay between clean metadata-capture repair batches.
const METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS: u64 = 60;

#[must_use]
fn metadata_capture_watch_interval(
    configured_interval: Option<u64>,
    stats: &download::SyncStats,
    cycle_failed_count: usize,
) -> Option<u64> {
    configured_interval.map(|interval| {
        if cycle_failed_count == 0
            && stats.metadata_capture_remaining > 0
            && stats.metadata_capture_progressed
            && !stats.has_rate_limit_pressure()
        {
            interval.min(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
        } else {
            interval
        }
    })
}

/// Decide whether to apply the service-mode watch-interval fallback.
///
/// Returns `Some(interval)` only when `service_mode` is true AND the
/// existing layered resolution (CLI > TOML > env) produced no value,
/// signaling that the daemon would otherwise run once and exit.
pub(crate) fn service_mode_default_interval(
    current: Option<u64>,
    service_mode: bool,
) -> Option<u64> {
    if service_mode && current.is_none() {
        Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
    } else {
        None
    }
}

/// Precondition and watch adjustment for `--refresh-metadata`. It is a one-shot
/// recovery pass, so it is rejected under `service run` and with any narrowing
/// filter that would strand out-of-scope rows. Returns `true` when the caller
/// must force a single one-shot cycle (clear the watch interval).
fn refresh_metadata_forces_one_shot(
    refresh_metadata: bool,
    service_mode: bool,
    narrows_enumeration: bool,
) -> anyhow::Result<bool> {
    if !refresh_metadata {
        return Ok(false);
    }
    anyhow::ensure!(
        !service_mode,
        "--refresh-metadata is a one-shot repair and is not supported under `kei service run`. Run `kei sync --refresh-metadata` instead."
    );
    anyhow::ensure!(
        !narrows_enumeration,
        "--refresh-metadata repairs every downloaded asset in the selected libraries and requires a complete library sweep. Enable the unfiled pass and remove album, smart-folder, media, or date/recent filters before retrying."
    );
    Ok(true)
}

fn validate_capture_timestamp_repair(
    repair: download::CaptureTimestampRepair,
    metadata: &config::MetadataConfig,
) -> anyhow::Result<download::CaptureTimestampRepair> {
    if matches!(repair, download::CaptureTimestampRepair::Preserve) {
        return Ok(repair);
    }
    anyhow::ensure!(
        metadata.set_exif_datetime,
        "--repair-capture-timestamps requires `metadata.set_exif_datetime = true` so the embedded datetime writer is enabled"
    );
    Ok(repair)
}

/// Keep explicit truncated-file replacement out of long-running service mode.
/// The operator must authorize each repair run from a foreground command.
fn repair_truncated_forces_one_shot(
    repair_truncated: bool,
    service_mode: bool,
) -> anyhow::Result<bool> {
    if !repair_truncated {
        return Ok(false);
    }
    anyhow::ensure!(
        !service_mode,
        "--repair-truncated is a one-shot repair and is not supported under `kei service run`. Run `kei sync --repair-truncated` instead."
    );
    Ok(true)
}

fn merge_refresh_tail_outcome(
    cycle_result: &mut crate::sync_cycle::CycleResult,
    failed: usize,
    interrupted: bool,
) {
    cycle_result.failed_count = cycle_result.failed_count.saturating_add(failed);
    cycle_result.stats.failed = cycle_result.stats.failed.saturating_add(failed);
    cycle_result.stats.interrupted |= interrupted;
}

fn sync_run_stats_from_cycle(cycle_result: &crate::sync_cycle::CycleResult) -> state::SyncRunStats {
    state::SyncRunStats {
        assets_seen: cycle_result.stats.assets_seen,
        assets_downloaded: u64::try_from(cycle_result.stats.downloaded).unwrap_or(u64::MAX),
        assets_failed: u64::try_from(cycle_result.stats.failed).unwrap_or(u64::MAX),
        enumeration_errors: u64::try_from(cycle_result.stats.enumeration_errors)
            .unwrap_or(u64::MAX),
        interrupted: cycle_result.stats.interrupted,
        api_total_at_start: cycle_result.stats.api_total_at_start,
        api_total_at_start_partial: cycle_result.stats.api_total_at_start_partial,
        inventory_drop_warnings: u64::try_from(cycle_result.stats.inventory_drop_warnings)
            .unwrap_or(u64::MAX),
        inventory_drop_previous_total: cycle_result.stats.inventory_drop_previous_total,
        inventory_drop_current_total: cycle_result.stats.inventory_drop_current_total,
        inventory_drop_library: cycle_result.stats.inventory_drop_library.clone(),
    }
}

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

/// Run the sync command: authenticate, enumerate photos, download, and
/// optionally loop in watch mode.
pub(crate) async fn run_sync(globals: &config::GlobalArgs, args: SyncArgs) -> anyhow::Result<()> {
    let capture = if args.sync.capture_icloud_responses {
        let data_dir = config::resolve_data_dir(
            globals.data_dir.as_deref(),
            args.toml_config.as_ref(),
            &args.config_path,
        );
        Some(crate::icloud::photos::capture::ResponseCapture::new(&data_dir).await?)
    } else {
        None
    };
    let result = Box::pin(run_sync_inner(globals, args, capture.as_ref())).await;
    if let Some(capture) = capture {
        // Some bounded producers outlive their consumer. Drain their in-flight
        // responses, and surface capture failures even when lookup policy caught them.
        capture.finish().await?;
    }
    result
}

async fn run_sync_inner(
    globals: &config::GlobalArgs,
    args: SyncArgs,
    capture: Option<&Arc<crate::icloud::photos::capture::ResponseCapture>>,
) -> anyhow::Result<()> {
    let check_capture = || capture.map_or(Ok(()), |capture| capture.check());
    let SyncArgs {
        is_one_shot,
        service_mode,
        pw,
        sync,
        toml_config,
        config_explicitly_set,
        config_path,
        redact_password,
        personality_mode,
        friendly_request,
        input_mode,
    } = args;

    let is_retry_failed = sync.retry_failed;
    let toml_existed = toml_config.is_some();
    let cli_data_dir = globals.data_dir.clone();
    let mut config = config::Config::build_inner(
        globals,
        &pw,
        sync,
        toml_config.as_ref(),
        personality_mode,
        friendly_request,
    )?;
    let capture_timestamp_repair = if config.runtime.repair_capture_timestamps {
        download::CaptureTimestampRepair::ReplaceWithCaptureLocal
    } else {
        download::CaptureTimestampRepair::Preserve
    };

    // On first run (no config file), persist bootstrap values so subsequent
    // runs don't need the same env again. Only when the user explicitly chose
    // a config path (--config), to avoid surprise writes at the default
    // location during tests or one-off runs.
    if !toml_existed
        && config_explicitly_set
        && let Err(e) =
            config::persist_first_run_config(&config_path, &config, cli_data_dir.as_deref())
    {
        tracing::warn!(error = %e, "Failed to save first-run config");
    }

    // One-shot operations — never inherit watch mode from TOML config,
    // which would cause the process to loop forever instead of exiting.
    // retry-failed: one-shot by definition.
    // setup → "sync now": initial test sync, not a daemon.
    if is_one_shot {
        config.watch.interval = None;
    }

    // --refresh-metadata is a one-shot recovery pass; reject watch/service and incomplete library sweeps, then force a single cycle.
    if refresh_metadata_forces_one_shot(
        config.runtime.refresh_metadata,
        service_mode,
        config.filters.narrows_enumeration(),
    )? {
        config.watch.interval = None;
    }
    let capture_timestamp_repair =
        validate_capture_timestamp_repair(capture_timestamp_repair, &config.metadata)?;
    if repair_truncated_forces_one_shot(config.runtime.repair_truncated, service_mode)? {
        config.watch.interval = None;
    }

    // Service-mode contract: the daemon must poll. If neither CLI nor TOML
    // supplied an interval, fall through to the canonical 24h default so a
    // launchd/systemd/SCM unit still has a heartbeat.
    if let Some(applied) = service_mode_default_interval(config.watch.interval, service_mode) {
        config.watch.interval = Some(applied);
        tracing::info!(
            interval_secs = applied,
            "service mode: applied default watch interval"
        );
    }
    let is_watch_mode = config.watch.interval.is_some();

    // Install password redaction now that we know the password
    if let Some(pw) = &config.auth.password
        && let Ok(mut guard) = redact_password.lock()
    {
        *guard = Some(SecretString::from(pw.expose_secret().to_owned()));
    }

    // Prevent core dumps from leaking in-memory credentials
    crate::harden_process();

    // Write PID file if requested (before auth so the PID is visible immediately)
    let _pid_guard = config
        .watch
        .pid_file
        .as_ref()
        .map(|p| PidFileGuard::new(p.clone()))
        .transpose()?;

    let sd_notifier = SystemdNotifier::new(config.watch.notify_systemd);
    let notifier = Notifier::new(config.notifications.script.clone());

    tracing::info!(concurrency = config.download.threads_num, "Starting kei");

    if config.auth.username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
    }

    // retry-failed + dry-run is unsupported: dry-run skips the state DB,
    // but retry-failed needs it to know which assets failed.
    if is_retry_failed && config.runtime.dry_run {
        anyhow::bail!(
            "`--dry-run` cannot be used with `--retry-failed` because retrying failed downloads needs to update the state database."
        );
    }

    // Validate download directory early (before auth) to avoid wasting a 2FA code
    // when the user simply forgot the destination.
    if config.download.directory.as_os_str().is_empty() {
        let message = crate::upgrade_hints::with_stale_env_hint(String::from(
            "Set [download].directory in the config file before syncing.",
        ));
        anyhow::bail!(message);
    }

    // Validate download directory is writable before spending time on authentication.
    tokio::fs::create_dir_all(&config.download.directory)
        .await
        .with_context(|| {
            format!(
                "Could not create download directory {}",
                config.download.directory.display()
            )
        })?;
    let probe = config.download.directory.join(".kei_probe");
    tokio::fs::write(&probe, b"").await.with_context(|| {
        format!(
            "Cannot write to download directory {}",
            config.download.directory.display()
        )
    })?;
    if let Err(e) = tokio::fs::remove_file(&probe).await {
        tracing::trace!(
            probe = %probe.display(),
            error = %e,
            "Could not clean up writability-probe file; harmless leakage"
        );
    }

    // Abort if available disk space is too low. See `check_min_disk_space`
    // for the pure inner check.
    if let Some(avail) = available_disk_space(&config.download.directory) {
        check_min_disk_space(avail, &config.download.directory)?;
    }

    #[cfg(debug_assertions)]
    if maybe_write_offline_fake_sync_report(&config, &notifier).await? {
        return Ok(());
    }

    let cred_store =
        credential::CredentialStore::new(&config.auth.username, &config.auth.cookie_directory);
    let source = password::build_password_source(
        config.auth.password.as_ref(),
        config.auth.password_command.as_deref(),
        config.auth.password_file.as_deref(),
        cred_store,
    );
    // Compute the save-password decision while `source` is still owned here.
    // The resulting action carries no password payload and survives moving the
    // source into the provider closure below.
    let save_password_action = config
        .auth
        .save_password
        .then(|| password::decide_save_password_action(&source));
    let password_provider = make_password_provider(source, input_mode);

    let auth_result = match auth::authenticate_with_modes(
        &config.auth.cookie_directory,
        &config.auth.username,
        &password_provider,
        config.auth.domain.as_str(),
        None,
        None,
        None,
        config.ui.personality_mode,
        input_mode,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(e) if classify_sync_auth_error(&e) == SyncAuthErrorClass::TwoFactorRequired => {
            let expected_generation = auth::two_factor_generation(&e);
            if !should_wait_for_2fa(is_watch_mode, &e) {
                return Err(e);
            }
            let msg = two_factor_recovery_message(&config.auth.username);
            tracing::warn!(message = %msg, "2FA required");
            notifier.notify(
                notifications::Event::TwoFaRequired,
                &msg,
                &config.auth.username,
                None,
                None,
            );

            wait_and_retry_2fa(&config.auth.cookie_directory, &config.auth.username, || {
                auth::authenticate_with_modes(
                    &config.auth.cookie_directory,
                    &config.auth.username,
                    &password_provider,
                    config.auth.domain.as_str(),
                    None,
                    None,
                    None,
                    config.ui.personality_mode,
                    input_mode,
                    expected_generation,
                )
            })
            .await?
        }
        Err(e) => return Err(e),
    };
    // Post-auth narration. Lands above any future bar; no-op in off mode.
    crate::personality::narration::auth_ok_to_stderr(
        config.ui.personality_mode,
        &config.auth.username,
    );

    // Save password to credential store if requested. Only the ephemeral
    // `Direct` source (CLI flag / env var) persists; File / Command /
    // Store / Interactive each emit a warning explaining why the flag is
    // a no-op for that source.
    if let Some(action) = save_password_action {
        match action {
            password::SavePasswordAction::Save => {
                if let Some(ref pw) = config.auth.password {
                    let store = credential::CredentialStore::new(
                        &config.auth.username,
                        &config.auth.cookie_directory,
                    );
                    if let Err(e) = store.store(pw.expose_secret()) {
                        tracing::warn!(error = %e, "Failed to save password to credential store");
                    } else {
                        tracing::info!(
                            backend = store.backend_name(),
                            "Password saved to credential store"
                        );
                    }
                }
            }
            password::SavePasswordAction::SkipWithWarning(reason) => {
                tracing::warn!(reason = %reason, "Skipping save of password to credential store");
            }
        }
    }

    let api_retry_config = retry::RetryConfig {
        max_retries: config.retry.max_retries,
        base_delay_secs: config.retry.retry_delay_secs,
        max_delay_secs: 60,
    };
    api_retry_config.validate()?;

    // CloudKit session/routing recovery: if init or the first CloudKit query
    // surfaces a session-error signature (401 stale session, or 421 persisting
    // after a pool reset), first retry the normal persisted-session auth path
    // before falling back to forced SRP. A second CloudKit failure bails cleanly
    // instead of looping under Docker's restart policy.
    let mut pending_auth = Some(auth_result);
    let mut retried_after_session_error = false;
    let (shared_session, mut photos_service, libraries) = loop {
        let this_auth = take_pending_auth(&mut pending_auth)?;
        let released_generation = this_auth.session.generation();
        let init_result = init_photos_service(
            this_auth,
            api_retry_config,
            config.ui.personality_mode,
            capture.cloned(),
        )
        .await;
        check_capture()?;
        let (ss, mut ps) = match init_result {
            Ok(pair) => pair,
            Err(e) if should_retry_session_init(&e, retried_after_session_error) => {
                tracing::warn!(
                    error = %e,
                    "CloudKit init failed with stale-session signature; retrying persisted-session authentication"
                );
                retried_after_session_error = true;
                pending_auth = Some(
                    reauth_after_session_error(
                        &config,
                        &password_provider,
                        &notifier,
                        None,
                        Some(released_generation),
                        is_watch_mode,
                        input_mode,
                    )
                    .await?,
                );
                continue;
            }
            Err(e) => return Err(e),
        };
        match resolve_libraries(&config.filters.selection.libraries, &mut ps).await {
            Ok(libs) => break (ss, ps, libs),
            Err(e) if should_retry_session_init(&e, retried_after_session_error) => {
                tracing::warn!(
                    error = %e,
                    "CloudKit returned stale-session signature; retrying persisted-session authentication"
                );
                retried_after_session_error = true;
                pending_auth = Some(
                    reauth_after_session_error(
                        &config,
                        &password_provider,
                        &notifier,
                        Some((ss, ps)),
                        None,
                        is_watch_mode,
                        input_mode,
                    )
                    .await?,
                );
            }
            Err(e) => return Err(e),
        }
    };
    tracing::debug!(
        count = libraries.len(),
        zones = %libraries.iter().map(|l| l.zone_name().to_string()).collect::<Vec<_>>().join(", "),
        "Resolved libraries"
    );
    // Post-library-resolve narration. Friendly-mode-only.
    crate::personality::narration::libraries_resolved_to_stderr(
        config.ui.personality_mode,
        libraries.len(),
    );

    // Initialize state database.
    // Skip for --dry-run so a preview doesn't create the DB or poison
    // sync tokens, which would cause a subsequent real sync to believe
    // nothing has changed and download 0 photos.
    let state_db: Option<Arc<dyn download::DownloadStore>> = if config.runtime.dry_run {
        None
    } else {
        let db_path = config.auth.cookie_directory.join(format!(
            "{}.db",
            auth::session::sanitize_username(&config.auth.username)
        ));
        match state::SqliteStateDb::open(&db_path).await {
            Ok(db) => {
                tracing::debug!(path = %db_path.display(), "State database opened");
                let db = Arc::new(db);

                // Promote any sync_runs rows left in status='running' from a
                // prior SIGKILL'd or crashed process. Runs once per process,
                // before any new sync starts.
                match db.promote_orphaned_sync_runs().await {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::warn!(
                            count,
                            "Promoted orphaned sync_runs rows to 'interrupted' \
                             (prior process exited uncleanly)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to promote orphaned sync_runs rows");
                    }
                }

                // Surface enum_in_progress:<zone> markers left by a prior
                // interrupted full enumeration so the operator understands
                // why the next full sync will re-enumerate from scratch.
                match db.list_interrupted_enumerations().await {
                    Ok(zones) if !zones.is_empty() => {
                        tracing::warn!(
                            zones = zones.join(","),
                            "Prior full enumeration was interrupted; next sync will re-enumerate \
                             the affected zones from offset 0"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            "Failed to list interrupted enumerations"
                        );
                    }
                }

                // For retry-failed, reset failed assets to pending
                if is_retry_failed {
                    match db.reset_failed().await {
                        Ok(0) => {
                            tracing::info!("No failed assets to retry");
                            return Ok(());
                        }
                        Ok(count) => {
                            tracing::debug!(count, "Reset failed assets to pending");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to reset failed assets");
                        }
                    }
                }

                Some(db as Arc<dyn download::DownloadStore>)
            }
            Err(e) => {
                anyhow::bail!("Could not open state database {}: {e}", db_path.display());
            }
        }
    };

    // First-sync notice: tell users on the `PrimarySync` default about any
    // shared libraries they could be syncing. Runs once per data dir,
    // gated by state DB metadata.
    maybe_notify_shared_libraries(
        &config.filters.selection.libraries,
        &mut photos_service,
        state_db
            .as_deref()
            .map(|db| db as &dyn state::SyncTokenStore),
    )
    .await;

    // Pre-compute config values used each cycle to build DownloadConfig.
    // DownloadConfig is rebuilt per-cycle so sync_mode can vary.
    let skip_created_before = config.filters.skip_created_before;
    let skip_created_after = config.filters.skip_created_after;
    let retry_config = api_retry_config;
    let live_resolution = config.photos.live_resolution.to_asset_version_size();
    // One shared limiter per sync run so the configured cap applies to
    // aggregate throughput across every concurrent download.
    let bandwidth_limiter = config
        .download
        .bandwidth_limit
        .map(download::limiter::BandwidthLimiter::new);
    if let Some(limiter) = &bandwidth_limiter {
        tracing::info!(
            bytes_per_sec = limiter.bytes_per_sec(),
            "Bandwidth limit enabled"
        );
    }
    // Promote the String / Vec / PathBuf config fields to their Arc
    // counterparts once, outside the per-library closure. Otherwise the
    // Arc-sharing win is half-defeated: each build_download_config
    // call would re-allocate directory / filename_exclude / temp_suffix
    // from scratch instead of refcount-bumping.
    let cfg_directory: Arc<std::path::Path> = Arc::from(config.download.directory.as_path());
    let cfg_filename_exclude: Arc<[glob::Pattern]> =
        Arc::from(config.download.filename_exclude.clone());
    let cfg_temp_suffix: Arc<str> = Arc::from(config.download.temp_suffix.as_str());
    let cfg_folder_structure_albums: Arc<str> =
        Arc::from(config.download.folder_structure_albums.as_str());
    let cfg_folder_structure_smart_folders: Arc<str> =
        Arc::from(config.download.folder_structure_smart_folders.as_str());
    let enum_config_hash: Arc<str> = download::compute_config_hash(&config).into();

    let build_download_config = |sync_mode: download::SyncMode,
                                 exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
                                 asset_groupings: Arc<download::AssetGroupings>,
                                 library: Arc<str>|
     -> Arc<download::DownloadConfig> {
        Arc::new(download::DownloadConfig {
            directory: Arc::clone(&cfg_directory),
            folder_structure: config.download.folder_structure.clone(),
            folder_structure_albums: Arc::clone(&cfg_folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&cfg_folder_structure_smart_folders),
            library,
            resolution: config.photos.resolution,
            media: config.filters.media,
            skip_created_before,
            skip_created_after,
            metadata: config.metadata,
            refresh_metadata: config.runtime.refresh_metadata,
            capture_timestamp_repair,
            repair_truncated: config.runtime.repair_truncated,
            concurrent_downloads: config.download.threads_num as usize,
            recent: config.filters.recent,
            recent_scope: config.filters.recent_scope,
            retry: retry_config,
            live_photo_mode: config.photos.live_photo_mode,
            live_resolution,
            live_photo_mov_filename_policy: config.photos.live_photo_mov_filename_policy,
            edited: config.photos.edited,
            alternative: config.photos.alternative,
            raw_policy: config.photos.raw_policy,
            file_match_policy: config.photos.file_match_policy,
            force_resolution: config.photos.force_resolution,
            keep_unicode_in_filenames: config.photos.keep_unicode_in_filenames,
            filename_exclude: Arc::clone(&cfg_filename_exclude),
            temp_suffix: Arc::clone(&cfg_temp_suffix),
            state_db: state_db.clone(),
            retry_only: is_retry_failed,
            max_download_attempts: config.retry.max_download_attempts,
            sync_mode,
            enum_config_hash: Some(Arc::clone(&enum_config_hash)),
            album_name: None,
            exclude_asset_ids,
            asset_groupings,
            bandwidth_limiter: bandwidth_limiter.clone(),
        })
    };
    let run_mode = if config.runtime.only_print_filenames {
        download::DownloadRunMode::PrintFilenames
    } else if config.runtime.dry_run {
        download::DownloadRunMode::DryRun
    } else {
        download::DownloadRunMode::Download
    };
    let download_controls = download::DownloadControls::new(
        run_mode,
        download::DownloadReporting::new(
            config.download.no_progress_bar,
            config.ui.personality_mode,
        ),
    );

    let shutdown_token = shutdown::install_signal_handler(sd_notifier, config.ui.personality_mode)?;

    // Suppress the tty driver's `^C` echo for the lifetime of this sync run.
    // Without this, the echoed `^C` overflows the bar's right-edge filler
    // and pushes the cursor down one line, making indicatif's next redraw
    // leave a stale top rule above the live bar. Friendly + tty only;
    // restored on Drop and on the second-Ctrl+C force-exit path. See
    // `personality::tty_echo` for the full context.
    let _echo_guard = if config.ui.personality_mode.is_friendly() {
        crate::personality::tty_echo::EchoGuard::install()
    } else {
        None
    };

    let mut reauth_attempts = 0u32;
    // Sum of per-cycle failed_counts across the lifetime of this process.
    // Surfaced at exit so watch-mode daemons don't mask earlier-cycle
    // failures behind a clean final cycle.
    let mut cumulative_failed_count = 0usize;

    let all_libraries = photos_service.all_libraries().await?;
    let cross_zone_libraries =
        resolve_cross_zone_libraries_for_album_hydration(&config.filters.selection, async {
            Ok::<_, anyhow::Error>(all_libraries.clone())
        })
        .await?;

    let collection_libraries =
        collection_libraries(&config.filters.selection, &libraries, &all_libraries);
    let collection_context =
        build_collection_context(&config.filters.selection, collection_libraries).await?;
    let selected_zones = zone_name_set(&libraries);
    let collection_zones = zone_name_set(collection_libraries);

    let mut library_states: Vec<LibraryState> = Vec::with_capacity(all_libraries.len());
    for library in &all_libraries {
        let zone_name = library.zone_name().to_string();
        let pass_scope = pass_scope_for_zone(
            &config.filters.selection,
            zone_name.as_str(),
            &selected_zones,
            &collection_zones,
        );
        if pass_scope.is_empty() {
            continue;
        }

        let sync_token_key = make_sync_token_key(&zone_name);
        let plan = resolve_passes_for_scope(
            library,
            &config.filters.selection,
            pass_scope,
            &collection_context,
            &cross_zone_libraries,
        )
        .await?;
        let (album_passes, smart_folder_passes, unfiled) = count_passes(&plan);
        tracing::info!(
            zone = %zone_name,
            album_passes,
            smart_folder_passes,
            unfiled,
            "Sync plan for library"
        );
        library_states.push(LibraryState {
            library: library.clone(),
            cross_zone_libraries: cross_zone_libraries.clone(),
            pass_scope,
            zone_name,
            sync_token_key,
            plan,
            plan_is_stale: false,
            plan_needs_refresh: false,
        });
    }
    warn_if_multi_library_paths_commingle(
        &library_states,
        &config.download.folder_structure,
        &config.download.folder_structure_albums,
        &config.download.folder_structure_smart_folders,
    );
    let db_precheck_scope = DbPrecheckScope::from_config(
        &config,
        &library_states,
        &build_download_config,
        enum_config_hash.as_ref(),
    )?;
    sd_notifier.notify_ready();
    let _systemd_watchdog_task = sd_notifier.start_watchdog_heartbeat(shutdown_token.clone());
    // Friendly-mode greeting. Lands above any future bar via
    // active_bar::with_suspended; no-op in off mode. Once per process.
    crate::personality::narration::greet_to_stderr(config.ui.personality_mode, is_watch_mode);

    // Spawn the HTTP server (/healthz + /metrics) only in watch mode.
    // A one-shot sync exits before anything could scrape /healthz, so there
    // is no value in binding the port. In watch mode, flag /healthz as stale
    // after two missed intervals so a single slow cycle doesn't flip to 503
    // but a stuck main loop does.
    // Binds synchronously so a misconfigured port fails at startup.
    let staleness_threshold = config
        .watch
        .interval
        .map(|secs| chrono::Duration::seconds((secs * 2) as i64));
    let (metrics_handle, metrics_task) = if config.watch.interval.is_some() {
        let (h, t, _addr) = crate::metrics::spawn_server(
            config.server.bind,
            config.server.port,
            shutdown_token.clone(),
            staleness_threshold,
        )?;
        (Some(h), Some(t))
    } else {
        (None, None)
    };

    let mut health = health::HealthStatus::new();
    let cycle_reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: is_watch_mode,
            report_path: config.report.json.as_deref(),
            run_options: crate::report::RunOptions::from_config(&config),
            health_dir: &config.auth.cookie_directory,
            personality_mode: config.ui.personality_mode,
            state_db: state_db.as_deref(),
            metrics_handle: metrics_handle.as_ref(),
            notifier: &notifier,
        });
    let mut consecutive_album_refresh_failures = 0u32;
    // 1-based cycle counter for periodic-reconcile cadence. Logged at
    // cycle start so an operator chasing missed reconciliation runs has a
    // breadcrumb. Cycle 1 is the first iteration of this loop, cycle 2 is the
    // first re-entry under `--watch`, etc.
    let mut cycle_index: u64 = 0;
    if is_watch_mode {
        match config.watch.reconcile_every_n_cycles {
            Some(n) if n > 0 => tracing::info!(
                every_n_cycles = n,
                "Periodic local-vs-state reconciliation enabled"
            ),
            _ => tracing::debug!(
                "Periodic local-vs-state reconciliation disabled \
                 (set [watch] reconcile_every_n_cycles in TOML to enable)"
            ),
        }
    }

    loop {
        check_capture()?;
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown requested, exiting...");
            break;
        }
        cycle_index = cycle_index.saturating_add(1);
        let mut next_watch_interval = config.watch.interval;

        // In watch mode with incremental sync, use changes/database as a
        // cheap pre-check before refreshing album plans or running a sync.
        // No-change cycles should cost one CloudKit request, not a full
        // album/pass refresh per selected library.
        let mut watch_precheck = if is_watch_mode {
            check_changes_database(
                state_db
                    .as_deref()
                    .map(|db| db as &dyn state::SyncTokenStore),
                &library_states,
                &mut photos_service,
                &db_precheck_scope,
            )
            .await
        } else {
            WatchPrecheck::proceed_all()
        };
        check_capture()?;

        if !config.runtime.dry_run
            && !config.runtime.only_print_filenames
            && let Some(db) = state_db.as_deref()
        {
            let drift = run_bounded_local_drift_probe(db, cycle_index).await;
            if drift.marked_failed > 0 {
                tracing::warn!(
                    marked_failed = drift.marked_failed,
                    "Local drift probe found missing or damaged files; forcing this cycle to retry them"
                );
                watch_precheck = WatchPrecheck::proceed_all();
            }
        }

        if is_watch_mode
            && !config.runtime.dry_run
            && !config.runtime.only_print_filenames
            && let Some(db) = state_db.as_deref()
        {
            include_pending_metadata_work(
                &mut watch_precheck,
                db,
                &config.metadata,
                &library_states,
            )
            .await;
        }

        if matches!(watch_precheck, WatchPrecheck::SkipAll) {
            cycle_reporter.report_skipped_watch_cycle(&mut health).await;
        } else {
            refresh_needed_library_plans(
                &mut library_states,
                &config.filters.selection,
                &collection_context,
                watch_precheck.changed_zones(),
                &mut consecutive_album_refresh_failures,
            )
            .await;
            check_capture()?;
            let cycle_library_states: Vec<&LibraryState> = library_states
                .iter()
                .filter(|s| watch_precheck.should_sync_zone(&s.zone_name))
                .collect();
            debug_assert!(!cycle_library_states.is_empty());

            sd_notifier.notify_status("Syncing...");
            sd_notifier.notify_watchdog();
            notifier.notify(
                notifications::Event::SyncStarted,
                "Sync cycle starting",
                &config.auth.username,
                None,
                None,
            );

            let cycle_started_at = std::time::Instant::now();
            let cycle_wall_started_at = chrono::Utc::now();
            let mut cycle_result = run_cycle(
                &cycle_library_states,
                &config,
                state_db.as_deref(),
                is_retry_failed,
                &build_download_config,
                download_controls,
                &shared_session,
                &shutdown_token,
            )
            .await?;
            check_capture()?;
            // Drain tagged rewrites now so the one-shot --refresh-metadata repair finishes in this run.
            let refresh_tail_failures = if config.runtime.refresh_metadata {
                if let Some(db) = state_db.as_deref() {
                    let library_scope: Vec<&str> = libraries
                        .iter()
                        .map(|library| library.zone_name())
                        .collect();
                    download::drain_pending_metadata_rewrites(
                        db,
                        &config.metadata,
                        capture_timestamp_repair,
                        &library_scope,
                        Arc::clone(&cfg_temp_suffix),
                        &shutdown_token,
                    )
                    .await
                } else {
                    0
                }
            } else {
                0
            };
            if config.runtime.refresh_metadata {
                merge_refresh_tail_outcome(
                    &mut cycle_result,
                    refresh_tail_failures,
                    shutdown_token.is_cancelled(),
                );
            }
            if refresh_tail_failures > 0 {
                tracing::warn!(
                    failed = refresh_tail_failures,
                    "Some --refresh-metadata rewrites did not complete; sync will exit non-zero"
                );
            }
            if let Some(db) = state_db.as_deref() {
                let stats = sync_run_stats_from_cycle(&cycle_result);
                match db.start_sync_run_at(cycle_wall_started_at).await {
                    Ok(run_id) => {
                        if let Err(e) = db.complete_sync_run(run_id, &stats).await {
                            tracing::warn!(
                                error = %e,
                                run_id,
                                "Failed to complete sync_runs ledger row"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to start sync_runs ledger row");
                    }
                }
            }

            if let Some(token) = watch_precheck.db_sync_token_after_success() {
                if !cycle_result.session_expired
                    && cycle_result.failed_count == 0
                    && !cycle_result.stats.interrupted
                    && cycle_result.db_sync_token_advance_safe
                {
                    if let Some(db) = state_db.as_deref() {
                        store_scoped_db_sync_token(
                            db as &dyn state::SyncTokenStore,
                            &db_precheck_scope,
                            token,
                        )
                        .await;
                    }
                } else {
                    tracing::debug!(
                        "changes/database token not advanced because the sync cycle did not complete cleanly"
                    );
                }
            }

            cycle_reporter
                .report_completed_cycle(
                    &mut health,
                    crate::cycle_reporter::CycleFacts::new(
                        &cycle_result.stats,
                        cycle_result.failed_count,
                        cycle_result.session_expired,
                        cycle_started_at.elapsed(),
                    ),
                )
                .await;

            next_watch_interval = metadata_capture_watch_interval(
                config.watch.interval,
                &cycle_result.stats,
                cycle_result.failed_count,
            );
            if let (Some(configured_interval), Some(follow_up_interval)) =
                (config.watch.interval, next_watch_interval)
                && follow_up_interval < configured_interval
            {
                tracing::info!(
                    refreshed = cycle_result.stats.metadata_capture_refreshed,
                    remaining = cycle_result.stats.metadata_capture_remaining,
                    interval_secs = follow_up_interval,
                    "Metadata-capture repair scheduled a bounded follow-up cycle"
                );
            }

            // Handle aggregate outcome across all libraries
            if cycle_result.session_expired {
                reauth_attempts += 1;
                if reauth_attempts >= MAX_REAUTH_ATTEMPTS {
                    anyhow::bail!(
                        "Your iCloud session expired and kei could not refresh it after {MAX_REAUTH_ATTEMPTS} attempts. Run `kei login` and try again."
                    );
                }
                tracing::warn!(
                    reauth_attempts,
                    max_attempts = MAX_REAUTH_ATTEMPTS,
                    "Session expired, attempting re-auth"
                );
                match attempt_reauth(
                    &shared_session,
                    &config.auth.cookie_directory,
                    &config.auth.username,
                    config.auth.domain.as_str(),
                    &password_provider,
                    input_mode,
                )
                .await
                {
                    Ok(()) => {
                        tracing::info!("Re-auth successful, resuming download...");
                        continue; // Restart entire cycle
                    }
                    Err(e)
                        if classify_sync_auth_error(&e)
                            == SyncAuthErrorClass::TwoFactorRequired =>
                    {
                        // 2FA is user action, not a failed attempt -- don't
                        // burn reauth_attempts so false wakeups from get-code
                        // can't exhaust the limit.
                        reauth_attempts -= 1;

                        let msg = two_factor_recovery_message(&config.auth.username);
                        tracing::warn!(message = %msg, "2FA required");
                        notifier.notify(
                            notifications::Event::TwoFaRequired,
                            &msg,
                            &config.auth.username,
                            None,
                            None,
                        );
                        if !should_wait_for_2fa(is_watch_mode, &e) {
                            return Err(e);
                        }

                        wait_and_retry_2fa(
                            &config.auth.cookie_directory,
                            &config.auth.username,
                            || {
                                attempt_reauth(
                                    &shared_session,
                                    &config.auth.cookie_directory,
                                    &config.auth.username,
                                    config.auth.domain.as_str(),
                                    &password_provider,
                                    input_mode,
                                )
                            },
                        )
                        .await?;
                        continue;
                    }
                    Err(e) => {
                        notifier.notify(
                            notifications::Event::SessionExpired,
                            &format!("Re-authentication failed: {e}"),
                            &config.auth.username,
                            None,
                            None,
                        );
                        return Err(e);
                    }
                }
            } else if cycle_result.failed_count > 0 {
                cumulative_failed_count =
                    cumulative_failed_count.saturating_add(cycle_result.failed_count);
                if is_watch_mode {
                    tracing::warn!(
                        failed_count = cycle_result.failed_count,
                        cumulative = cumulative_failed_count,
                        "Some sync failures occurred this cycle, will retry next cycle"
                    );
                } else {
                    return Err(PartialSyncError(cycle_result.failed_count).into());
                }
            } else {
                reauth_attempts = 0;
            }
        }

        // Periodic local-vs-state reconciliation. This full-catalog walk is
        // read-only and surfaces missing or damaged files via `tracing::warn!`.
        // The bounded pre-cycle probe owns automatic requeue for the rows it
        // samples; `kei reconcile` remains the explicit full repair command
        // for operators who want to sweep the whole state DB immediately.
        if is_watch_mode
            && should_reconcile_this_cycle(cycle_index, config.watch.reconcile_every_n_cycles)
            && let Some(db) = state_db.as_ref()
        {
            run_periodic_reconcile(db.as_ref() as &dyn state::ReportStateStore, cycle_index).await;
        }

        if let Some(interval) = next_watch_interval {
            check_capture()?;
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, exiting...");
                break;
            }

            // Release the file lock during idle sleep so that docker exec
            // commands (login get-code, login submit-code) can acquire it.
            {
                let session = shared_session.read().await;
                if let Err(e) = session.release_lock() {
                    tracing::warn!(error = %e, "Failed to release lock before idle sleep");
                }
            }

            sd_notifier.notify_status(&format!("Waiting {interval} seconds..."));
            tracing::info!(interval_secs = interval, "Waiting before next cycle");
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                () = shutdown_token.cancelled() => {
                    tracing::info!("Shutdown during wait, exiting...");
                    break;
                }
            }

            // Validate session before next cycle; re-authenticate if expired.
            reacquire_session(&shared_session, &config, &password_provider, input_mode).await?;

            // Mark album/pass plans stale after an idle sleep, but defer the
            // CloudKit refresh until `changes/database` says a selected
            // library actually has work. Quiet watch cycles can then go back
            // to sleep without listing albums for every selected library.
            for lib_state in &mut library_states {
                lib_state.plan_needs_refresh = true;
            }
        } else {
            break;
        }
    }

    // Friendly farewell line. Only on graceful Ctrl+C-driven exit: if the
    // loop fell through without `shutdown_token` ever being cancelled (e.g.
    // a one-shot run completing normally), the per-cycle signoff already
    // covered the closing sentiment and a second "Done." line would be noise.
    if shutdown_token.is_cancelled() {
        crate::personality::narration::farewell_to_stderr(config.ui.personality_mode);
    }

    // Signal the metrics server to shut down (idempotent if SIGINT already
    // fired) and await its graceful drain so the binary doesn't exit while
    // an in-flight /metrics scrape is still flushing.
    if let Some(task) = metrics_task {
        shutdown_token.cancel();
        if let Err(e) = task.await {
            tracing::warn!(error = %e, "metrics server task panicked");
        }
    }

    // Exit non-zero if any cycle in this watch session had failures, not
    // just the last one. A single successful final cycle must not mask a
    // multi-cycle failure backlog in Docker / systemd exit-code signalling.
    if cumulative_failed_count > 0 {
        Err(PartialSyncError(cumulative_failed_count).into())
    } else {
        Ok(())
    }
}

// Debug-only seam for the offline binary-boundary report test. Release builds
// must never skip authentication or CloudKit work from an environment variable.
#[cfg(debug_assertions)]
async fn maybe_write_offline_fake_sync_report(
    config: &config::Config,
    notifier: &Notifier,
) -> anyhow::Result<bool> {
    const ENV: &str = "KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS";
    if std::env::var(ENV).as_deref() != Ok("1") {
        return Ok(false);
    }

    let Some(report_path) = config.report.json.as_deref() else {
        anyhow::bail!("{ENV}=1 requires [report].json");
    };

    tracing::warn!(
        env = ENV,
        "Offline fake sync report test seam enabled; exiting before authentication"
    );

    let reporter = crate::cycle_reporter::CycleReporter::<state::SqliteStateDb>::new(
        crate::cycle_reporter::CycleReporterConfig {
            watch_mode: config.watch.interval.is_some(),
            report_path: Some(report_path),
            run_options: crate::report::RunOptions::from_config(config),
            health_dir: &config.auth.cookie_directory,
            personality_mode: config.ui.personality_mode,
            state_db: None,
            metrics_handle: None,
            notifier,
        },
    );
    let stats = download::SyncStats {
        assets_seen: 3,
        downloaded: 2,
        skipped: download::SkipBreakdown {
            by_state: 1,
            ..download::SkipBreakdown::default()
        },
        bytes_downloaded: 4096,
        disk_bytes_written: 4096,
        elapsed_secs: 0.125,
        photos_downloaded: 1,
        videos_downloaded: 1,
        ..download::SyncStats::default()
    };
    let mut health = health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::new(
                &stats,
                0,
                false,
                std::time::Duration::from_millis(125),
            ),
        )
        .await;

    if !report_path.is_file() {
        anyhow::bail!("offline fake sync did not write {}", report_path.display());
    }

    Ok(true)
}

/// Re-authenticate after a session-error signature from CloudKit.
///
/// Drops any live session + service (releasing the file lock), removes only the
/// validation cache, then retries normal authentication so `/accountLogin` can
/// consume persisted session state. If that cannot recover, falls back to the
/// older forced-SRP path by stripping routing state. 2FA-required errors get one
/// final persisted-session retry, then notify and wait for
/// `kei login submit-code`.
async fn reauth_after_session_error(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    live: Option<(auth::SharedSession, crate::icloud::photos::PhotosService)>,
    released_generation: Option<auth::session::SessionGeneration>,
    is_watch_mode: bool,
    input_mode: crate::InputMode,
) -> anyhow::Result<auth::AuthResult> {
    let expected_generation = if let Some((ss, ps)) = live {
        let session = ss.read().await;
        let generation = session.generation();
        session.release_lock()?;
        drop(session);
        drop(ps);
        drop(ss);
        Some(generation)
    } else {
        released_generation
    };

    clear_validation_cache_for_reauth(&config.auth.cookie_directory, &config.auth.username).await;
    match authenticate_sync_session(config, password_provider, input_mode, expected_generation)
        .await
    {
        Ok(result) => return Ok(result),
        Err(e)
            if matches!(
                classify_sync_auth_error(&e),
                SyncAuthErrorClass::SessionReset | SyncAuthErrorClass::LockContention
            ) =>
        {
            return Err(e);
        }
        Err(e) if classify_sync_auth_error(&e) == SyncAuthErrorClass::TwoFactorRequired => {
            let expected_generation = auth::two_factor_generation(&e).or(expected_generation);
            if let Some(result) = retry_persisted_session_after_two_factor(
                config,
                password_provider,
                input_mode,
                expected_generation,
            )
            .await?
            {
                return Ok(result);
            }
            if !should_wait_for_2fa(is_watch_mode, &e) {
                return Err(e);
            }
            return notify_and_wait_for_2fa(
                config,
                password_provider,
                notifier,
                input_mode,
                expected_generation,
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Persisted-session authentication did not recover; forcing SRP re-authentication"
            );
        }
    }

    auth::strip_session_routing_state(
        &config.auth.cookie_directory,
        &config.auth.username,
        expected_generation,
    )
    .await?;

    match authenticate_sync_session(config, password_provider, input_mode, expected_generation)
        .await
    {
        Ok(result) => Ok(result),
        Err(e) if classify_sync_auth_error(&e) == SyncAuthErrorClass::SessionReset => Err(e),
        Err(e) if classify_sync_auth_error(&e) == SyncAuthErrorClass::TwoFactorRequired => {
            let expected_generation = auth::two_factor_generation(&e).or(expected_generation);
            if let Some(result) = retry_persisted_session_after_two_factor(
                config,
                password_provider,
                input_mode,
                expected_generation,
            )
            .await?
            {
                return Ok(result);
            }
            if !should_wait_for_2fa(is_watch_mode, &e) {
                return Err(e);
            }
            notify_and_wait_for_2fa(
                config,
                password_provider,
                notifier,
                input_mode,
                expected_generation,
            )
            .await
        }
        Err(e) => Err(e),
    }
}

async fn clear_validation_cache_for_reauth(cookie_dir: &std::path::Path, username: &str) {
    let cache_path = auth::validation_cache_file_path(cookie_dir, username);
    match tokio::fs::remove_file(&cache_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::debug!(
                path = %cache_path.display(),
                error = %e,
                "Could not remove validation cache before session recovery"
            );
        }
    }
}

async fn authenticate_sync_session(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    input_mode: crate::InputMode,
    expected_generation: Option<auth::session::SessionGeneration>,
) -> anyhow::Result<auth::AuthResult> {
    auth::authenticate_with_modes(
        &config.auth.cookie_directory,
        &config.auth.username,
        password_provider,
        config.auth.domain.as_str(),
        None,
        None,
        None,
        config.ui.personality_mode,
        input_mode,
        expected_generation,
    )
    .await
}

async fn retry_persisted_session_after_two_factor(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    input_mode: crate::InputMode,
    expected_generation: Option<auth::session::SessionGeneration>,
) -> anyhow::Result<Option<auth::AuthResult>> {
    tracing::debug!(
        "2FA-required auth wrote session state; retrying persisted-session auth once before waiting"
    );
    clear_validation_cache_for_reauth(&config.auth.cookie_directory, &config.auth.username).await;
    match authenticate_sync_session(config, password_provider, input_mode, expected_generation)
        .await
    {
        Ok(result) => Ok(Some(result)),
        Err(e) if classify_sync_auth_error(&e) == SyncAuthErrorClass::SessionReset => Err(e),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "Persisted-session retry after 2FA-required auth did not recover"
            );
            Ok(None)
        }
    }
}

async fn notify_and_wait_for_2fa(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    input_mode: crate::InputMode,
    expected_generation: Option<auth::session::SessionGeneration>,
) -> anyhow::Result<auth::AuthResult> {
    let msg = two_factor_recovery_message(&config.auth.username);
    tracing::warn!(message = %msg, "2FA required");
    notifier.notify(
        notifications::Event::TwoFaRequired,
        &msg,
        &config.auth.username,
        None,
        None,
    );
    wait_and_retry_2fa(&config.auth.cookie_directory, &config.auth.username, || {
        authenticate_sync_session(config, password_provider, input_mode, expected_generation)
    })
    .await
}

/// Walk every `downloaded` row in the state DB and warn when the
/// recorded `local_path` is missing or shorter than expected. Read-only - no
/// rows are mutated by this periodic full-catalog walk. Triggered on a fixed
/// cadence by the watch loop so operators can see drift outside the bounded
/// pre-cycle probe.
///
/// Errors from the DB scan are logged at `warn!` rather than propagated:
/// the periodic walk is a diagnostic, not a load-bearing correctness gate,
/// and a transient SQLite hiccup must not crash the watch daemon.
async fn run_periodic_reconcile(db: &dyn state::ReportStateStore, cycle_index: u64) {
    use crate::commands::reconcile::{LocalDriftAsset, LocalDriftKind, scan_local_drift};
    tracing::info!(
        cycle_index,
        "Periodic reconciliation: scanning state DB for missing or damaged local files"
    );
    let mut sample_logged = 0usize;
    const SAMPLE_LOG_CAP: usize = 25;
    // Cap per-cycle log spam at SAMPLE_LOG_CAP missing entries; the
    // aggregate count is logged below regardless of how many fired.
    let report_drift = |m: &LocalDriftAsset| {
        if sample_logged < SAMPLE_LOG_CAP {
            match m.kind {
                LocalDriftKind::Missing => tracing::warn!(
                    asset_id = %m.id,
                    version_size = m.version_size.as_str(),
                    path = %m.local_path.display(),
                    "Reconcile: state row marks asset downloaded but local file is missing"
                ),
                LocalDriftKind::Truncated {
                    actual_size,
                    expected_size,
                } => tracing::warn!(
                    asset_id = %m.id,
                    version_size = m.version_size.as_str(),
                    path = %m.local_path.display(),
                    actual_size,
                    expected_size,
                    "Reconcile: state row marks asset downloaded but local file is smaller than expected"
                ),
            }
            sample_logged += 1;
        }
    };
    let report_no_path = |id: &str| {
        tracing::debug!(asset_id = %id, "Reconcile: downloaded row has no local_path recorded");
    };
    let scan = scan_local_drift(db, report_drift, report_no_path).await;
    match scan {
        Ok((counts, drifted)) => {
            if drifted.is_empty() && counts.no_path == 0 {
                tracing::info!(
                    present = counts.present,
                    "Periodic reconciliation: all downloaded files look present on disk"
                );
            } else {
                tracing::warn!(
                    present = counts.present,
                    missing = counts.missing,
                    damaged = counts.damaged,
                    no_path = counts.no_path,
                    sample_logged,
                    "Periodic reconciliation: drift detected; run `kei reconcile` to mark local drift for re-download"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Periodic reconciliation scan failed; will retry on next interval");
        }
    }
}

const LOCAL_DRIFT_PROBE_CURSOR_KEY: &str = "local_drift_probe_offset_v1";
const LOCAL_DRIFT_PROBE_PAGE_SIZE: u32 = 128;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LocalDriftProbeOutcome {
    scanned: u64,
    drifted: u64,
    marked_failed: u64,
    mark_errors: u64,
}

/// Probe a bounded page of downloaded rows for local drift before each sync
/// cycle. Unlike the opt-in full reconciliation walk, this runs by default
/// and advances a cursor so watch mode eventually covers the catalog without
/// turning every quiet incremental cycle into a full filesystem crawl.
async fn run_bounded_local_drift_probe(
    db: &dyn download::DownloadStore,
    cycle_index: u64,
) -> LocalDriftProbeOutcome {
    let summary = match db.get_summary().await {
        Ok(summary) => summary,
        Err(e) => {
            tracing::warn!(error = %e, "Local drift probe failed to read state summary");
            return LocalDriftProbeOutcome::default();
        }
    };
    if summary.downloaded == 0 {
        return LocalDriftProbeOutcome::default();
    }

    let start_offset = match db.get_metadata(LOCAL_DRIFT_PROBE_CURSOR_KEY).await {
        Ok(Some(raw)) => raw.parse::<u64>().unwrap_or(0).min(summary.downloaded),
        Ok(None) => 0,
        Err(e) => {
            tracing::warn!(error = %e, "Local drift probe failed to read cursor");
            0
        }
    };

    let mut page = match db
        .get_downloaded_page(start_offset, LOCAL_DRIFT_PROBE_PAGE_SIZE)
        .await
    {
        Ok(page) => page,
        Err(e) => {
            tracing::warn!(error = %e, "Local drift probe failed to load downloaded page");
            return LocalDriftProbeOutcome::default();
        }
    };
    let offset = if page.is_empty() && start_offset > 0 {
        match db.get_downloaded_page(0, LOCAL_DRIFT_PROBE_PAGE_SIZE).await {
            Ok(first_page) => {
                page = first_page;
                0
            }
            Err(e) => {
                tracing::warn!(error = %e, "Local drift probe failed to wrap cursor");
                return LocalDriftProbeOutcome::default();
            }
        }
    } else {
        start_offset
    };

    let scanned = u64::try_from(page.len()).unwrap_or(u64::MAX);
    let next_offset = if page.is_empty()
        || offset.saturating_add(scanned) >= summary.downloaded
        || scanned < u64::from(LOCAL_DRIFT_PROBE_PAGE_SIZE)
    {
        0
    } else {
        offset.saturating_add(scanned)
    };
    if let Err(e) = db
        .set_metadata(LOCAL_DRIFT_PROBE_CURSOR_KEY, &next_offset.to_string())
        .await
    {
        tracing::warn!(error = %e, "Local drift probe failed to persist cursor");
    }

    let mut outcome = LocalDriftProbeOutcome {
        scanned,
        ..LocalDriftProbeOutcome::default()
    };
    for asset in page {
        let (drift, no_path) = match crate::commands::reconcile::classify_local_drift(asset).await {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(error = %e, "Local drift probe failed to inspect a downloaded row");
                continue;
            }
        };
        if no_path {
            continue;
        }
        let Some(drift) = drift else {
            continue;
        };
        outcome.drifted = outcome.drifted.saturating_add(1);
        match drift.kind {
            crate::commands::reconcile::LocalDriftKind::Missing => tracing::warn!(
                cycle_index,
                asset_id = %drift.id,
                version_size = drift.version_size.as_str(),
                path = %drift.local_path.display(),
                "Local drift probe found a missing downloaded file"
            ),
            crate::commands::reconcile::LocalDriftKind::Truncated {
                actual_size,
                expected_size,
            } => tracing::warn!(
                cycle_index,
                asset_id = %drift.id,
                version_size = drift.version_size.as_str(),
                path = %drift.local_path.display(),
                actual_size,
                expected_size,
                "Local drift probe found a truncated downloaded file"
            ),
        }
        match db
            .mark_failed(
                &drift.library,
                &drift.id,
                drift.version_size.as_str(),
                drift.kind.reason(),
            )
            .await
        {
            Ok(()) => outcome.marked_failed = outcome.marked_failed.saturating_add(1),
            Err(e) => {
                outcome.mark_errors = outcome.mark_errors.saturating_add(1);
                tracing::warn!(
                    error = %e,
                    asset_id = %drift.id,
                    version_size = drift.version_size.as_str(),
                    "Local drift probe could not mark drifted file failed"
                );
            }
        }
    }

    if outcome.drifted > 0 || outcome.mark_errors > 0 {
        tracing::warn!(
            cycle_index,
            scanned = outcome.scanned,
            drifted = outcome.drifted,
            marked_failed = outcome.marked_failed,
            mark_errors = outcome.mark_errors,
            next_offset,
            "Local drift probe completed with drift"
        );
    } else {
        tracing::debug!(
            cycle_index,
            scanned = outcome.scanned,
            next_offset,
            "Local drift probe completed"
        );
    }
    outcome
}

/// Should this watch cycle run a periodic local-vs-state reconciliation?
///
/// Returns `true` for the very first cycle whose 1-based index is a multiple
/// of `every_n` (e.g. `every_n = 24` fires on cycle 24, 48, ...). The first
/// firing is at cycle `every_n` rather than cycle 0 so a freshly-started
/// daemon doesn't burn its startup time walking the disk before a single
/// sync has run. Disabled (`None`) or `Some(0)` always returns `false`; the
/// `cycle_index` is also 1-based so the first cycle is `1`.
///
/// Pure function so the cadence is unit-testable without spinning up a real
/// watch loop or filesystem walk.
pub(crate) fn should_reconcile_this_cycle(cycle_index: u64, every_n: Option<u64>) -> bool {
    let n = match every_n {
        Some(n) if n > 0 => n,
        _ => return false,
    };
    cycle_index > 0 && cycle_index.is_multiple_of(n)
}

/// Decide whether the reauth path inside the sync loop should block on a
/// 2FA prompt or surface the error to the caller.
///
/// In **watch mode** a 2FA-required error is recoverable: the loop notifies
/// the user, parks `wait_and_retry_2fa`, and resumes once a code arrives.
///
/// In **one-shot mode** there is nothing to wait on -- the caller (a CI run,
/// a cron, the systemd unit's first start) needs the error so it can exit
/// non-zero and the operator can run `kei login get-code`.
///
/// The initial-auth, provider-session recovery, and mid-cycle reauth paths all
/// use this predicate. Keeping the wait decision in the sync owner prevents a
/// foreground error from being reclassified globally as service success.
pub(crate) fn should_wait_for_2fa(is_watch_mode: bool, err: &anyhow::Error) -> bool {
    is_watch_mode && classify_sync_auth_error(err) == SyncAuthErrorClass::TwoFactorRequired
}

async fn refresh_needed_library_plans(
    library_states: &mut [LibraryState],
    selection: &crate::selection::Selection,
    collection_context: &CollectionContext,
    changed_zones: Option<&rustc_hash::FxHashSet<String>>,
    consecutive_album_refresh_failures: &mut u32,
) {
    for lib_state in library_states {
        if !lib_state.plan_needs_refresh {
            continue;
        }
        if changed_zones.is_some_and(|zones| !zones.contains(&lib_state.zone_name)) {
            continue;
        }

        // Re-resolve albums per-library to discover newly created iCloud albums.
        // Full sync resolves unfiled album-member exclusions in the download
        // phase; incremental/cleanup paths resolve them before planning tasks.
        // This refresh is intentionally delayed until a selected zone has
        // changes so quiet watch cycles avoid the album-listing traffic.
        match resolve_passes_for_scope(
            &lib_state.library,
            selection,
            lib_state.pass_scope,
            collection_context,
            &lib_state.cross_zone_libraries,
        )
        .await
        {
            Ok(refreshed) => {
                lib_state.plan = refreshed;
                lib_state.plan_is_stale = false;
                lib_state.plan_needs_refresh = false;
                *consecutive_album_refresh_failures = 0;
            }
            Err(e) => {
                *consecutive_album_refresh_failures += 1;
                lib_state.plan_is_stale = true;
                lib_state.plan_needs_refresh = true;
                if *consecutive_album_refresh_failures >= 3 {
                    tracing::error!(
                        zone = %lib_state.zone_name,
                        error = %e,
                        consecutive_failures = *consecutive_album_refresh_failures,
                        "Repeated album refresh failures, reusing previous set"
                    );
                } else {
                    tracing::warn!(
                        zone = %lib_state.zone_name,
                        error = %e,
                        "Failed to refresh albums, reusing previous set"
                    );
                }
            }
        }
    }
}

async fn store_scoped_db_sync_token(
    db: &dyn state::SyncTokenStore,
    scope: &DbPrecheckScope,
    token: &str,
) {
    if let Err(e) = db
        .upsert_scoped_db_sync_token(scope.to_state_row(token))
        .await
    {
        tracing::warn!(error = %e, "Failed to store scoped db sync token");
    }
}

/// Check `changes/database` to determine if this watch cycle can be skipped.
///
/// Returns `SkipAll` when a complete pre-check reports no selected-zone changes.
/// An empty complete page still skips the cycle but keeps the previous
/// scoped DB token, so the next watch wakeup rechecks from the same point.
async fn check_changes_database(
    state_db: Option<&dyn state::SyncTokenStore>,
    library_states: &[LibraryState],
    photos_service: &mut crate::icloud::photos::PhotosService,
    scope: &DbPrecheckScope,
) -> WatchPrecheck {
    let Some(db) = state_db else {
        return WatchPrecheck::proceed_all();
    };
    if library_states.is_empty() {
        return WatchPrecheck::SkipAll;
    }
    let scoped_token = match db
        .get_scoped_db_sync_token(
            &scope.provider,
            &scope.account,
            scope.shape_version,
            &scope.scope_hash,
        )
        .await
    {
        Ok(Some(token)) if !token.token.trim().is_empty() => token,
        Ok(_) => {
            return match photos_service.changes_database(None).await {
                Ok(db_resp) if !db_resp.more_coming => WatchPrecheck::Proceed {
                    changed_zones: None,
                    db_sync_token_after_success: Some(db_resp.sync_token),
                },
                Ok(db_resp) => {
                    tracing::debug!(
                        zones = db_resp.zones.len(),
                        "changes/database bootstrap had more pages; scoped db sync token not stored"
                    );
                    WatchPrecheck::proceed_all()
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "changes/database bootstrap failed; proceeding with sync"
                    );
                    WatchPrecheck::proceed_all()
                }
            };
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                scope_hash = %scope.scope_hash,
                "Failed to read scoped changes/database sync token; proceeding with sync"
            );
            return WatchPrecheck::proceed_all();
        }
    };
    if serde_json::from_str::<serde_json::Value>(&scoped_token.scope_json).is_err() {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database scope JSON is invalid; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }
    if scoped_token.scope_json != scope.scope_json {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database scope JSON mismatch; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }
    if serde_json::from_str::<Vec<String>>(&scoped_token.selected_zones_json).is_err() {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database selected-zone JSON is invalid; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }
    if scoped_token.selected_zones_json != scope.selected_zones_json {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database selected zones mismatch; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }

    match photos_service
        .changes_database(Some(scoped_token.token.as_str()))
        .await
    {
        Ok(db_resp) => {
            let selected_zones: rustc_hash::FxHashSet<&str> = library_states
                .iter()
                .map(|s| s.zone_name.as_str())
                .collect();
            let mut changed_selected_zones = rustc_hash::FxHashSet::default();
            let has_any_changed_zone = !db_resp.zones.is_empty();
            if db_resp.more_coming {
                tracing::debug!("changes/database has more pages (moreComing=true)");
            }
            for z in &db_resp.zones {
                tracing::debug!(
                    zone = %z.zone_id.zone_name,
                    zone_sync_token = %z.sync_token,
                    "changes/database: zone has changes"
                );
                if selected_zones.contains(z.zone_id.zone_name.as_str()) {
                    changed_selected_zones.insert(z.zone_id.zone_name.clone());
                }
            }

            if changed_selected_zones.is_empty() {
                if db_resp.more_coming {
                    return WatchPrecheck::Proceed {
                        changed_zones: None,
                        db_sync_token_after_success: Some(db_resp.sync_token),
                    };
                }
                if has_any_changed_zone {
                    store_scoped_db_sync_token(db, scope, &db_resp.sync_token).await;
                } else {
                    tracing::debug!(
                        "changes/database returned an empty complete page; skipping without advancing scoped db sync token"
                    );
                }
                tracing::info!(
                    "No selected library changes detected (changes/database), skipping cycle"
                );
                return WatchPrecheck::SkipAll;
            }

            WatchPrecheck::Proceed {
                changed_zones: if db_resp.more_coming {
                    None
                } else {
                    Some(changed_selected_zones)
                },
                db_sync_token_after_success: Some(db_resp.sync_token),
            }
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                "changes/database pre-check failed, proceeding with sync"
            );
            WatchPrecheck::proceed_all()
        }
    }
}

/// Identify active-pass templates that lack `{library}` when multiple
/// libraries are selected. Returns a sorted list of CLI flag names whose
/// templates would let same-named assets from different zones land in the
/// same on-disk path. Empty list means the multi-library plan is unambiguous.
///
/// The check is per-pass-kind: each *active* template (one whose pass kind
/// will actually run under the current Selection) must contain `{library}`.
pub(crate) fn count_passes(plan: &crate::commands::AlbumPlan) -> (usize, usize, bool) {
    use crate::commands::PassKind;
    let mut album = 0;
    let mut smart_folder = 0;
    let mut unfiled = false;
    for pass in &plan.passes {
        match pass.kind {
            PassKind::Album => album += 1,
            PassKind::SmartFolder => smart_folder += 1,
            PassKind::Unfiled => unfiled = true,
        }
    }
    (album, smart_folder, unfiled)
}

/// Template strings whose pass is disabled (e.g. `folder_structure_smart_folders`
/// when `--smart-folder none`) don't render any path so they don't need
/// `{library}` to keep the sync safe.
fn find_multi_library_commingle_flags(
    library_states: &[LibraryState],
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
) -> Vec<&'static str> {
    if library_states.len() < 2 {
        return Vec::new();
    }

    let mut active_unfiled_libraries = 0usize;
    let mut active_album_libraries = 0usize;
    let mut active_smart_folder_libraries = 0usize;
    for state in library_states {
        let (album_passes, smart_folder_passes, has_unfiled_pass) = count_passes(&state.plan);
        if has_unfiled_pass {
            active_unfiled_libraries += 1;
        }
        if album_passes > 0 {
            active_album_libraries += 1;
        }
        if smart_folder_passes > 0 {
            active_smart_folder_libraries += 1;
        }
    }

    // All passes disabled - resolve_passes returns an empty plan, no path
    // ever renders, multi-library can't commingle.
    if active_unfiled_libraries == 0
        && active_album_libraries == 0
        && active_smart_folder_libraries == 0
    {
        return Vec::new();
    }

    let mut missing: Vec<&'static str> = Vec::new();
    if active_unfiled_libraries > 1 && !folder_structure.contains("{library}") {
        missing.push("--folder-structure");
    }
    if active_album_libraries > 1 && !folder_structure_albums.contains("{library}") {
        missing.push("--folder-structure-albums");
    }
    if active_smart_folder_libraries > 1 && !folder_structure_smart_folders.contains("{library}") {
        missing.push("--folder-structure-smart-folders");
    }
    missing
}

/// Emit a startup warning when multi-library paths commingle. Informational:
/// the run continues, and `file_match_policy` (default
/// `name-size-dedup-with-suffix`) keeps two libraries from silently
/// overwriting each other -- collisions land at `<name>-1.<ext>` rather
/// than overwriting. The warning surfaces the namespace ambiguity so the
/// user can add `{library}` to their templates if they want zone-disjoint
/// trees.
fn warn_if_multi_library_paths_commingle(
    library_states: &[LibraryState],
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
) {
    let missing = find_multi_library_commingle_flags(
        library_states,
        folder_structure,
        folder_structure_albums,
        folder_structure_smart_folders,
    );
    if missing.is_empty() {
        return;
    }
    let library_count = library_states.len();
    tracing::warn!(
        library_count,
        missing = ?missing,
        "Multi-library sync: active template(s) lack `{{library}}`; same-named \
         assets from different zones will share an on-disk namespace. \
         File-match policy keeps writes from overwriting (collisions get a \
         `-N` suffix), but cross-library files end up interleaved. Add \
         `{{library}}` to each listed template for zone-disjoint trees."
    );
}

/// Re-acquire the lock after idle sleep, then re-validate the session.
async fn reacquire_session(
    shared_session: &auth::SharedSession,
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    reacquire_session_lock_after_idle(shared_session).await?;

    if let Err(e) = attempt_reauth(
        shared_session,
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
        password_provider,
        input_mode,
    )
    .await
    {
        tracing::warn!(error = %e, "Pre-cycle reauth failed, will retry mid-sync");
        reacquire_session_lock_after_idle(shared_session).await?;
    }

    Ok(())
}

async fn reacquire_session_lock_after_idle(
    shared_session: &auth::SharedSession,
) -> anyhow::Result<()> {
    let session = shared_session.read().await;
    let Err(e) = session.reacquire_lock() else {
        return Ok(());
    };

    if classify_sync_auth_error(&e) == SyncAuthErrorClass::LockContention {
        tracing::error!(
            error = %e,
            "Another kei process acquired the session lock while watch mode slept; stopping before the next sync cycle"
        );
    } else {
        tracing::error!(
            error = %e,
            "Failed to reacquire the session lock after watch sleep"
        );
    }
    Err(e.context(
        "Could not regain the iCloud session lock after watch mode slept. Stopping so another kei process cannot use the same session at the same time.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_mode_default_locked_at_one_day() {
        // The roadmap promises the kei daemon polls every 24h out of the
        // box. Regression-guard the constant so a casual rename doesn't
        // silently shorten the interval and 10x the API call rate.
        assert_eq!(SERVICE_MODE_DEFAULT_WATCH_INTERVAL, 86_400);
    }

    #[test]
    fn service_mode_default_applied_when_no_other_source_set_interval() {
        assert_eq!(
            service_mode_default_interval(None, true),
            Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
        );
    }

    #[test]
    fn service_mode_default_skipped_when_cli_or_toml_already_set_interval() {
        // A user-provided interval (CLI / TOML / env) must always win;
        // service mode never silently overrides an explicit choice.
        assert_eq!(service_mode_default_interval(Some(60), true), None);
        assert_eq!(service_mode_default_interval(Some(3600), true), None);
    }

    #[test]
    fn service_mode_default_skipped_outside_service_mode() {
        // Plain `kei sync` must remain single-shot when no interval is
        // configured. Only the service entry point applies the fallback.
        assert_eq!(service_mode_default_interval(None, false), None);
    }

    #[test]
    fn progressing_metadata_capture_shortens_the_watch_interval() {
        let default_interval = Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL);
        let stats = download::SyncStats {
            metadata_capture_refreshed: 500,
            metadata_capture_remaining: 701,
            metadata_capture_progressed: true,
            ..download::SyncStats::default()
        };

        assert_eq!(
            metadata_capture_watch_interval(default_interval, &stats, 0),
            Some(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
        );
        assert_eq!(
            metadata_capture_watch_interval(Some(60), &stats, 0),
            Some(60)
        );
        assert_eq!(metadata_capture_watch_interval(None, &stats, 0), None);

        let absorbed_rate_limit = download::SyncStats {
            assets_seen: 100,
            rate_limited: 9,
            ..stats
        };
        assert_eq!(
            metadata_capture_watch_interval(default_interval, &absorbed_rate_limit, 0),
            Some(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
        );

        let deleted_batch = download::SyncStats {
            metadata_capture_remaining: 1,
            metadata_capture_progressed: true,
            ..download::SyncStats::default()
        };
        assert_eq!(
            metadata_capture_watch_interval(default_interval, &deleted_batch, 0),
            Some(METADATA_CAPTURE_FOLLOW_UP_INTERVAL_SECS)
        );
    }

    #[test]
    fn stalled_or_failed_metadata_capture_keeps_the_configured_interval() {
        for (stats, cycle_failed_count) in [
            (
                download::SyncStats {
                    metadata_capture_remaining: 701,
                    ..download::SyncStats::default()
                },
                0,
            ),
            (
                download::SyncStats {
                    metadata_capture_refreshed: 499,
                    metadata_capture_failures: 1,
                    metadata_capture_remaining: 701,
                    metadata_capture_progressed: true,
                    ..download::SyncStats::default()
                },
                1,
            ),
            (
                download::SyncStats {
                    metadata_capture_refreshed: 500,
                    metadata_capture_progressed: true,
                    ..download::SyncStats::default()
                },
                0,
            ),
        ] {
            assert_eq!(
                metadata_capture_watch_interval(
                    Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL),
                    &stats,
                    cycle_failed_count,
                ),
                Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
            );
        }
    }

    #[test]
    fn rate_limited_metadata_capture_keeps_the_configured_interval() {
        for stats in [
            download::SyncStats {
                rate_limited: 1,
                metadata_capture_remaining: 701,
                metadata_capture_progressed: true,
                ..download::SyncStats::default()
            },
            download::SyncStats {
                assets_seen: 100,
                rate_limited: 10,
                metadata_capture_remaining: 701,
                metadata_capture_progressed: true,
                ..download::SyncStats::default()
            },
        ] {
            assert_eq!(
                metadata_capture_watch_interval(
                    Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL),
                    &stats,
                    0,
                ),
                Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
            );
        }
    }

    #[test]
    fn watch_precheck_skip_all_blocks_every_zone_and_token() {
        let precheck = WatchPrecheck::SkipAll;

        assert!(precheck.changed_zones().is_none());
        assert!(precheck.db_sync_token_after_success().is_none());
        assert!(!precheck.should_sync_zone("PrimarySync"));
        assert!(!precheck.should_sync_zone("SharedSync-123"));
    }

    #[test]
    fn watch_precheck_proceed_all_allows_every_zone_without_db_token() {
        let precheck = WatchPrecheck::proceed_all();

        assert!(precheck.changed_zones().is_none());
        assert!(precheck.db_sync_token_after_success().is_none());
        assert!(precheck.should_sync_zone("PrimarySync"));
        assert!(precheck.should_sync_zone("SharedSync-123"));
    }

    #[test]
    fn watch_precheck_changed_zones_scopes_sync_and_carries_db_token() {
        let mut zones = rustc_hash::FxHashSet::default();
        zones.insert("PrimarySync".to_string());
        let precheck = WatchPrecheck::Proceed {
            changed_zones: Some(zones),
            db_sync_token_after_success: Some("db-token-after-cycle".to_string()),
        };

        assert_eq!(
            precheck.db_sync_token_after_success(),
            Some("db-token-after-cycle")
        );
        assert_eq!(
            precheck
                .changed_zones()
                .expect("changed zone filter should be present")
                .len(),
            1
        );
        assert!(precheck.should_sync_zone("PrimarySync"));
        assert!(!precheck.should_sync_zone("SharedSync-123"));
    }

    #[test]
    fn pending_metadata_work_bypasses_skip_for_only_affected_zone() {
        let mut precheck = WatchPrecheck::SkipAll;
        let mut local_zones = rustc_hash::FxHashSet::default();
        local_zones.insert("SharedSync-123".to_string());

        precheck.include_local_work_zones(local_zones);

        assert!(!precheck.should_sync_zone("PrimarySync"));
        assert!(precheck.should_sync_zone("SharedSync-123"));
        assert!(precheck.db_sync_token_after_success().is_none());
    }

    #[test]
    fn pending_metadata_work_joins_provider_changed_zones_without_losing_db_token() {
        let mut changed_zones = rustc_hash::FxHashSet::default();
        changed_zones.insert("PrimarySync".to_string());
        let mut precheck = WatchPrecheck::Proceed {
            changed_zones: Some(changed_zones),
            db_sync_token_after_success: Some("db-token-after-cycle".to_string()),
        };
        let mut local_zones = rustc_hash::FxHashSet::default();
        local_zones.insert("SharedSync-123".to_string());

        precheck.include_local_work_zones(local_zones);

        assert!(precheck.should_sync_zone("PrimarySync"));
        assert!(precheck.should_sync_zone("SharedSync-123"));
        assert_eq!(
            precheck.db_sync_token_after_success(),
            Some("db-token-after-cycle")
        );
    }

    async fn seed_watch_metadata_work(
        db: &state::SqliteStateDb,
        library: &str,
        asset_id: &str,
        capture_revision: i64,
    ) {
        let record = crate::test_helpers::TestAssetRecord::new(asset_id)
            .library(library)
            .filename(&format!("{asset_id}.jpg"))
            .build();
        db.upsert_seen(&record).await.expect("seed asset");
        db.mark_downloaded(
            library,
            asset_id,
            "original",
            std::path::Path::new("unused-watch-precheck-photo.jpg"),
            "local-checksum",
            None,
        )
        .await
        .expect("mark asset downloaded");
        db.set_metadata_capture_revision_for_test(library, asset_id, capture_revision);
    }

    #[tokio::test]
    async fn disabled_metadata_writers_leave_rewrite_marker_out_of_watch_work() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        seed_watch_metadata_work(
            &db,
            "PrimarySync",
            "REWRITE_DISABLED",
            state::METADATA_CAPTURE_REVISION,
        )
        .await;
        db.record_metadata_write_failure("PrimarySync", "REWRITE_DISABLED", "original")
            .await
            .expect("seed rewrite marker");
        let library = make_run_cycle_library_state("PrimarySync", "sync_token", "zone_token");
        let mut precheck = WatchPrecheck::SkipAll;

        include_pending_metadata_work(
            &mut precheck,
            &db,
            &config::MetadataConfig::default(),
            &[library],
        )
        .await;

        assert!(!precheck.should_sync_zone("PrimarySync"));
        assert_eq!(
            db.get_pending_metadata_rewrites_page(None, 0, 10)
                .await
                .expect("read rewrite markers")
                .len(),
            1,
            "disabling writers must retain durable retry evidence"
        );
    }

    #[tokio::test]
    async fn enabled_metadata_writer_forces_only_selected_rewrite_zone() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        for (library, asset_id) in [
            ("PrimarySync", "REWRITE_SELECTED"),
            ("SharedSync-OTHER", "REWRITE_UNSELECTED"),
        ] {
            seed_watch_metadata_work(&db, library, asset_id, state::METADATA_CAPTURE_REVISION)
                .await;
            db.record_metadata_write_failure(library, asset_id, "original")
                .await
                .expect("seed rewrite marker");
        }
        let selected = make_run_cycle_library_state("PrimarySync", "sync_token", "zone_token");
        let metadata = config::MetadataConfig {
            set_exif_rating: true,
            ..config::MetadataConfig::default()
        };
        let mut precheck = WatchPrecheck::SkipAll;

        include_pending_metadata_work(&mut precheck, &db, &metadata, &[selected]).await;

        assert!(precheck.should_sync_zone("PrimarySync"));
        assert!(!precheck.should_sync_zone("SharedSync-OTHER"));
    }

    #[tokio::test]
    async fn capture_revision_forces_watch_work_with_metadata_writers_disabled() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        seed_watch_metadata_work(
            &db,
            "PrimarySync",
            "CAPTURE_PENDING",
            state::METADATA_CAPTURE_REVISION - 1,
        )
        .await;
        let library = make_run_cycle_library_state("PrimarySync", "sync_token", "zone_token");
        let mut precheck = WatchPrecheck::SkipAll;

        include_pending_metadata_work(
            &mut precheck,
            &db,
            &config::MetadataConfig::default(),
            &[library],
        )
        .await;

        assert!(precheck.should_sync_zone("PrimarySync"));
    }

    #[tokio::test]
    async fn watch_reacquire_lock_failure_stops_before_next_cycle() {
        let (dir, shared_session) = make_shared_session_for_run_cycle().await;
        shared_session.read().await.release_lock().unwrap();
        let _holder = auth::session::Session::new(
            dir.path(),
            "test@example.com",
            "https://example.com",
            None,
        )
        .await
        .expect("second session should acquire released lock");

        let result = reacquire_session_lock_after_idle(&shared_session).await;

        assert!(
            result.is_err(),
            "watch mode must not start another sync cycle after lock contention"
        );
        let err = result.expect_err("lock contention should stop watch mode");
        assert!(
            err.downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_lock_contention),
            "expected LockContention, got: {err:#}"
        );
        assert!(
            format!("{err:#}").contains("Stopping so another kei process"),
            "error should explain why watch mode stopped: {err:#}"
        );
    }

    #[tokio::test]
    async fn watch_lock_release_still_allows_reacquire_success() {
        let (dir, shared_session) = make_shared_session_for_run_cycle().await;
        shared_session.read().await.release_lock().unwrap();

        reacquire_session_lock_after_idle(&shared_session)
            .await
            .expect("watch mode should reacquire an uncontended session lock");

        let result = auth::session::Session::new(
            dir.path(),
            "test@example.com",
            "https://example.com",
            None,
        )
        .await;
        match result {
            Ok(_) => panic!("reacquired watch lock should block a second session"),
            Err(err) => assert!(
                err.downcast_ref::<auth::error::AuthError>()
                    .is_some_and(auth::error::AuthError::is_lock_contention),
                "watch mode should hold the session lock after reacquire: {err:#}"
            ),
        }
    }

    fn primary() -> crate::selection::LibrarySelector {
        crate::selection::LibrarySelector::default()
    }

    fn all_libraries() -> crate::selection::LibrarySelector {
        crate::selection::parse_library_selector(&["all".to_string()]).unwrap()
    }

    fn shared_zone() -> crate::selection::LibrarySelector {
        crate::selection::parse_library_selector(&["SharedSync-ABCD1234".to_string()]).unwrap()
    }

    fn selection_with_smart_folder(
        libraries: crate::selection::LibrarySelector,
        unfiled: bool,
    ) -> crate::selection::Selection {
        use crate::selection::{AlbumSelector, Selection, SmartFolderSelector};
        Selection {
            albums: AlbumSelector::None,
            albums_explicit: false,
            smart_folders: SmartFolderSelector::Named {
                included: std::collections::BTreeSet::from(["Hidden".to_string()]),
                excluded: std::collections::BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries,
            unfiled,
        }
    }

    fn test_library(zone_name: &str) -> crate::icloud::photos::PhotoLibrary {
        crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(crate::test_helpers::MockPhotosSession::new()),
            zone_name,
        )
    }

    #[test]
    fn run_sync_scope_planning_shared_only_smart_folder_widens_zone_scope() {
        let selection = selection_with_smart_folder(
            crate::selection::parse_library_selector(&["shared".to_string()]).unwrap(),
            false,
        );
        let primary = test_library("PrimarySync");
        let shared = test_library("SharedSync-ABCD1234");
        let selected_libraries = vec![shared.clone()];
        let all_libraries = vec![primary.clone(), shared.clone()];

        let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
        let selected_zones = zone_name_set(&selected_libraries);
        let collection_zones = zone_name_set(collection);

        let primary_scope = pass_scope_for_zone(
            &selection,
            primary.zone_name(),
            &selected_zones,
            &collection_zones,
        );
        let shared_scope = pass_scope_for_zone(
            &selection,
            shared.zone_name(),
            &selected_zones,
            &collection_zones,
        );

        assert!(
            primary_scope.include_smart_folders,
            "explicit smart-folder selection should widen pass planning beyond the library selector"
        );
        assert!(
            shared_scope.include_smart_folders,
            "run_sync planning should schedule smart-folder passes for selected shared zone"
        );
    }

    #[test]
    fn run_sync_scope_planning_primary_only_still_filters_unfiled() {
        let selection = selection_with_smart_folder(
            crate::selection::parse_library_selector(&["primary".to_string()]).unwrap(),
            true,
        );
        let primary = test_library("PrimarySync");
        let shared = test_library("SharedSync-ABCD1234");
        let selected_libraries = vec![primary.clone()];
        let all_libraries = vec![primary.clone(), shared.clone()];

        let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
        let selected_zones = zone_name_set(&selected_libraries);
        let collection_zones = zone_name_set(collection);

        let primary_scope = pass_scope_for_zone(
            &selection,
            primary.zone_name(),
            &selected_zones,
            &collection_zones,
        );
        let shared_scope = pass_scope_for_zone(
            &selection,
            shared.zone_name(),
            &selected_zones,
            &collection_zones,
        );

        assert!(
            primary_scope.include_smart_folders,
            "run_sync planning should keep smart-folder passes in the selected primary zone"
        );
        assert!(
            shared_scope.include_smart_folders,
            "explicit smart-folder selection should widen scope to shared zones too"
        );
        assert!(
            primary_scope.include_unfiled,
            "selected primary zone should keep unfiled pass when unfiled=true"
        );
        assert!(
            !shared_scope.include_unfiled,
            "library selector should still filter unfiled passes to selected zones"
        );
    }

    #[test]
    fn notice_suppressed_when_already_shown() {
        // The marker overrides everything: even a user with 5 shared libraries
        // and the default selection gets no notice on re-entry.
        assert!(should_notify_shared_libraries(&primary(), 5, true).is_none());
    }

    #[test]
    fn notice_suppressed_when_no_shared_libraries() {
        assert!(should_notify_shared_libraries(&primary(), 0, false).is_none());
    }

    #[test]
    fn notice_suppressed_when_user_picked_all() {
        // Anyone who explicitly set all libraries has already opted in;
        // nothing to tell them.
        assert!(should_notify_shared_libraries(&all_libraries(), 3, false).is_none());
    }

    #[test]
    fn notice_suppressed_when_user_picked_shared_zone_explicitly() {
        // A user who configured `SharedSync-ABCD1234` has also made
        // a choice; don't second-guess them.
        assert!(should_notify_shared_libraries(&shared_zone(), 3, false).is_none());
    }

    #[test]
    fn notice_fires_with_singular_wording_for_one_library() {
        let msg = should_notify_shared_libraries(&primary(), 1, false).unwrap();
        assert!(
            msg.contains("1 iCloud shared library"),
            "singular 'library' wording expected; got: {msg}"
        );
        assert!(
            msg.contains("is being synced"),
            "singular verb 'is' expected; got: {msg}"
        );
        // The guidance is what the notice is for - it must name the config
        // key, the CLI flag, and the discovery subcommand.
        assert!(
            msg.contains("[filters] libraries = [\"all\"]"),
            "TOML guidance missing: {msg}"
        );
        assert!(
            !msg.contains("--library all"),
            "CLI guidance should be gone: {msg}"
        );
        assert!(
            msg.contains("kei list libraries"),
            "discovery guidance missing: {msg}"
        );
    }

    #[test]
    fn notice_fires_with_plural_wording_for_multiple_libraries() {
        let msg = should_notify_shared_libraries(&primary(), 3, false).unwrap();
        assert!(
            msg.contains("3 iCloud shared libraries"),
            "plural 'libraries' wording expected; got: {msg}"
        );
        assert!(
            msg.contains("are being synced"),
            "plural verb 'are' expected; got: {msg}"
        );
    }

    #[test]
    fn notice_suppressed_when_both_user_opted_out_and_already_notified() {
        // Belt-and-braces: every suppression condition stacks correctly.
        assert!(should_notify_shared_libraries(&all_libraries(), 0, true).is_none());
    }

    #[test]
    fn shared_library_notice_recent_check_uses_ttl() {
        let now = 1_800_000_000;
        assert!(shared_library_notice_recently_checked(
            Some(&(now - 60).to_string()),
            now
        ));
        assert!(!shared_library_notice_recently_checked(
            Some(&(now - SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS - 1).to_string()),
            now
        ));
        assert!(!shared_library_notice_recently_checked(
            Some("not-a-ts"),
            now
        ));
    }

    #[derive(Clone)]
    struct CountingSharedLibrarySession {
        shared_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for CountingSharedLibrarySession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/shared/zones/list") {
                self.shared_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(serde_json::json!({"zones": []}));
            }
            anyhow::bail!("unexpected URL: {url}")
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn shared_notice_service(
        calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> crate::icloud::photos::PhotosService {
        crate::icloud::photos::PhotosService::for_testing(
            Box::new(CountingSharedLibrarySession {
                shared_calls: calls,
            }),
            std::collections::HashMap::new(),
        )
    }

    #[tokio::test]
    async fn shared_library_notice_skips_uncached_dry_run_probe_without_state_db() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut service = shared_notice_service(Arc::clone(&calls));

        maybe_notify_shared_libraries(&primary(), &mut service, None).await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "without a state DB the shared-library probe cannot be cached, so dry-run should not pay the API call"
        );
    }

    #[tokio::test]
    async fn shared_library_notice_caches_no_shared_libraries() {
        let db = make_state_db();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut first = shared_notice_service(Arc::clone(&calls));
        maybe_notify_shared_libraries(&primary(), &mut first, Some(db.as_ref())).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let mut second = shared_notice_service(Arc::clone(&calls));
        maybe_notify_shared_libraries(&primary(), &mut second, Some(db.as_ref())).await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fresh no-shared marker should suppress the next shared-zone listing"
        );
    }

    // The run_sync reauth retry branch keys on whether an error from
    // init_photos_service or resolve_libraries is a session error.
    // Misclassifying either retries SRP on a non-session failure
    // (burning an Apple rate-limit slot) or fails to retry on a real
    // 401/421 (visible as an immediate Docker restart). Pin every
    // variant so a future ICloudError refactor can't silently regress.

    #[test]
    fn is_session_error_true_for_cloudkit_401_403() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 401 }.into();
        assert!(is_session_error(&e), "401 must trigger reauth");

        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 403 }.into();
        assert!(is_session_error(&e), "403 must trigger reauth");
    }

    #[test]
    fn is_session_error_true_for_cloudkit_421() {
        let e: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(
            is_session_error(&e),
            "persistent 421 must trigger persisted-session auth before forced SRP"
        );
    }

    #[test]
    fn is_session_error_false_for_service_not_activated() {
        // ADP / ZONE_NOT_FOUND is a permanent failure, not a session issue.
        // Reauth would burn an Apple rate-limit slot for nothing.
        let e: anyhow::Error = crate::icloud::error::ICloudError::ServiceNotActivated {
            code: "ADP".into(),
            reason: "Advanced Data Protection".into(),
        }
        .into();
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_false_for_connection_and_io() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::Connection("DNS failure".into()).into();
        assert!(!is_session_error(&e));

        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x");
        let e: anyhow::Error = crate::icloud::error::ICloudError::from(io).into();
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_false_for_non_icloud_error() {
        // Any other anyhow error (config parsing, state DB, etc.) must not
        // be classified as a session error — that would trigger an
        // inappropriate SRP cycle.
        let e = anyhow::anyhow!("unrelated top-level error");
        assert!(!is_session_error(&e));
    }

    fn classify_sync_auth_error_for(err: auth::error::AuthError) -> SyncAuthErrorClass {
        let err = anyhow::Error::new(err);
        classify_sync_auth_error(&err)
    }

    #[test]
    fn classify_sync_auth_error_detects_two_factor_required() {
        assert_eq!(
            classify_sync_auth_error_for(auth::error::AuthError::TwoFactorRequired),
            SyncAuthErrorClass::TwoFactorRequired
        );
    }

    #[test]
    fn classify_sync_auth_error_detects_lock_contention() {
        assert_eq!(
            classify_sync_auth_error_for(auth::error::AuthError::LockContention(
                "session.lock".into()
            )),
            SyncAuthErrorClass::LockContention
        );
    }

    #[test]
    fn classify_sync_auth_error_detects_session_reset() {
        assert_eq!(
            classify_sync_auth_error_for(auth::error::AuthError::SessionReset),
            SyncAuthErrorClass::SessionReset
        );
    }

    #[test]
    fn classify_sync_auth_error_treats_failed_login_and_plain_anyhow_as_other() {
        assert_eq!(
            classify_sync_auth_error_for(auth::error::AuthError::FailedLogin(
                "bad password".into()
            )),
            SyncAuthErrorClass::Other
        );

        let err = anyhow::anyhow!("plain failure");
        assert_eq!(classify_sync_auth_error(&err), SyncAuthErrorClass::Other);
    }

    #[test]
    fn classify_sync_auth_error_detects_context_wrapped_auth_errors() {
        let two_factor =
            anyhow::Error::new(auth::error::AuthError::TwoFactorRequired).context("initial auth");
        assert_eq!(
            classify_sync_auth_error(&two_factor),
            SyncAuthErrorClass::TwoFactorRequired
        );

        let lock = anyhow::Error::new(auth::error::AuthError::LockContention(
            "session.lock".into(),
        ))
        .context("watch idle reacquire");
        assert_eq!(
            classify_sync_auth_error(&lock),
            SyncAuthErrorClass::LockContention
        );
    }

    #[test]
    fn is_session_error_peers_through_context() {
        // Real error chains are wrapped in .context() before hitting the
        // retry branch. The classifier downcasts on the root cause, which
        // anyhow exposes as downcast_ref — wrap here to pin the contract.
        let root = crate::icloud::error::ICloudError::SessionExpired { status: 401 };
        let e = anyhow::Error::from(root).context("while initializing photos service");
        assert!(
            is_session_error(&e),
            "classifier must downcast through context wrappers"
        );
    }

    /// Comprehensive classification table: every `ICloudError` variant plus
    /// a generic anyhow error. Prevents silent regressions from future enum
    /// additions — adding a new variant requires updating this table.
    #[test]
    fn is_session_error_classification_table() {
        use crate::icloud::error::ICloudError;

        // Variants that SHOULD trigger re-auth (session errors). Each entry
        // is a (label, anyhow::Error) pair so downcast_ref inside
        // is_session_error works correctly without needing Clone.
        let session_errors: Vec<(&str, anyhow::Error)> = vec![
            (
                "SessionExpired-401",
                ICloudError::SessionExpired { status: 401 }.into(),
            ),
            (
                "SessionExpired-403",
                ICloudError::SessionExpired { status: 403 }.into(),
            ),
            ("MisdirectedRequest", ICloudError::MisdirectedRequest.into()),
        ];
        for (label, e) in session_errors {
            assert!(
                is_session_error(&e),
                "expected {label} to be a session error"
            );
        }

        // Variants that must NOT trigger re-auth (non-session errors).
        let non_session: Vec<(&str, anyhow::Error)> = vec![
            (
                "Connection",
                ICloudError::Connection("DNS timeout".into()).into(),
            ),
            (
                "ServiceNotActivated",
                ICloudError::ServiceNotActivated {
                    code: "ZONE_NOT_FOUND".into(),
                    reason: "ADP".into(),
                }
                .into(),
            ),
            (
                "Io",
                ICloudError::from(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
                .into(),
            ),
            (
                "Json",
                ICloudError::from(
                    serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
                )
                .into(),
            ),
            ("non-ICloudError", anyhow::anyhow!("config parse error")),
        ];
        for (label, e) in non_session {
            assert!(
                !is_session_error(&e),
                "expected {label} to NOT be a session error"
            );
        }
    }

    // Classifier sees through `Box<dyn Error + Send + Sync>` wrappers.
    //
    // anyhow lets callers wrap raw boxed errors via `anyhow::Error::from`
    // when adapting third-party error returns. The downcast walk used by
    // `is_session_error` must conclude "not a session error" for any error
    // chain that was never an `ICloudError` — otherwise a future refactor
    // through a boxed-error adapter could silently flip every config /
    // network / state error into "burn an Apple SRP slot".
    #[test]
    fn is_session_error_through_boxed_error_returns_false() {
        // A boxed error of a foreign type wrapped via anyhow::Error::from.
        let boxed: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("foreign"));
        let e: anyhow::Error = anyhow::Error::from_boxed(boxed);
        assert!(
            !is_session_error(&e),
            "boxed-error wrapper must not be classified as a session error"
        );

        // Also: anyhow::Error::msg(string) must be non-session — this is
        // the most common path through our error pipeline (a `bail!` from
        // a non-icloud module).
        let plain = anyhow::anyhow!("config parse failed at line 7");
        assert!(
            !is_session_error(&plain),
            "free-form anyhow::Error must not be classified as a session error"
        );
    }

    // ── find_multi_library_commingle_flags ───────────────────────────
    //
    // Multi-library sync without a `{library}` token lets same-named
    // assets from different zones share an on-disk namespace. The
    // `file_match_policy` keeps writes from silently overwriting (the
    // default policy adds a `-N` suffix on collision), but the user
    // probably wanted zone-disjoint trees. `warn_if_*` emits a startup
    // warning; these tests pin the underlying `find_*` truth table.

    /// Build a Selection that activates every pass kind. The default
    /// `LibrarySelector` is fine — the guard reads only `albums`,
    /// `smart_folders`, `unfiled` from the Selection.
    fn selection_all_passes_active() -> crate::selection::Selection {
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        Selection {
            albums: AlbumSelector::All {
                excluded: std::collections::BTreeSet::new(),
            },
            albums_explicit: true,
            smart_folders: SmartFolderSelector::All {
                include_sensitive: false,
                excluded: std::collections::BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries: LibrarySelector::default(),
            unfiled: true,
        }
    }

    /// Build a Selection that activates only the unfiled pass.
    fn selection_unfiled_only() -> crate::selection::Selection {
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        Selection {
            albums: AlbumSelector::None,
            albums_explicit: false,
            smart_folders: SmartFolderSelector::None,
            smart_folders_explicit: false,
            libraries: LibrarySelector::default(),
            unfiled: true,
        }
    }

    fn commingle_test_states(
        count: usize,
        selection: &crate::selection::Selection,
    ) -> Vec<LibraryState> {
        use crate::commands::PassKind;
        use crate::selection::{AlbumSelector, SmartFolderSelector};

        let pass_scope = PassScope {
            include_albums: !matches!(selection.albums, AlbumSelector::None),
            include_smart_folders: !matches!(selection.smart_folders, SmartFolderSelector::None),
            include_unfiled: selection.unfiled,
        };
        (0..count)
            .map(|idx| {
                let zone_name = if idx == 0 {
                    "PrimarySync".to_string()
                } else {
                    format!("SharedSync-{:08X}", idx)
                };
                let library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                    Box::new(crate::test_helpers::MockPhotosSession::new()),
                    &zone_name,
                );
                LibraryState {
                    library,
                    cross_zone_libraries: Vec::new(),
                    pass_scope,
                    zone_name: zone_name.clone(),
                    sync_token_key: format!("sync_token:{zone_name}"),
                    plan: crate::commands::AlbumPlan {
                        passes: [
                            pass_scope
                                .include_albums
                                .then(|| make_pass("album", PassKind::Album)),
                            pass_scope
                                .include_smart_folders
                                .then(|| make_pass("smart-folder", PassKind::SmartFolder)),
                            pass_scope
                                .include_unfiled
                                .then(|| make_pass("unfiled", PassKind::Unfiled)),
                        ]
                        .into_iter()
                        .flatten()
                        .collect(),
                    },
                    plan_is_stale: false,
                    plan_needs_refresh: false,
                }
            })
            .collect()
    }

    #[test]
    fn find_multi_library_commingle_flags_short_circuits_under_two_libraries() {
        // Zero or one library never flags any template, regardless of
        // template content or active-pass selection.
        let sel = selection_all_passes_active();
        let states0 = commingle_test_states(0, &sel);
        assert!(
            find_multi_library_commingle_flags(&states0, "%Y/%m/%d", "{album}", "{smart-folder}")
                .is_empty()
        );
        let states1 = commingle_test_states(1, &sel);
        assert!(
            find_multi_library_commingle_flags(&states1, "%Y/%m/%d", "{album}", "{smart-folder}")
                .is_empty()
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_accepts_library_token_in_active_template_only() {
        // CG-7 contract: when every active template carries `{library}`,
        // multi-library is safe. Inactive templates are irrelevant -
        // their pass kind doesn't run.
        let all = selection_all_passes_active();
        let all_states = commingle_test_states(2, &all);
        assert!(
            find_multi_library_commingle_flags(
                &all_states,
                "{library}/%Y/%m/%d",
                "{library}/{album}",
                "{library}/{smart-folder}",
            )
            .is_empty(),
            "every active template carries `{{library}}` - no commingle"
        );

        // When only the unfiled pass is active, the unfiled template is
        // the only one that needs `{library}`. The album / smart-folder
        // templates can be anything because no pass reads them.
        let unfiled = selection_unfiled_only();
        let unfiled_states = commingle_test_states(2, &unfiled);
        assert!(
            find_multi_library_commingle_flags(
                &unfiled_states,
                "{library}/%Y/%m/%d",
                "{album}",
                "{smart-folder}",
            )
            .is_empty(),
            "only unfiled active and its template has `{{library}}`"
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_reports_per_active_pass() {
        // CG-6 contract: only *active* passes whose templates lack
        // `{library}` show up in the missing-flags list.
        //
        // Scenario: --folder-structure-albums '{library}/{album}' (token
        // present, active) + --folder-structure-smart-folders
        // '{smart-folder}' (no token, active because --smart-folder all)
        // + --unfiled false (inactive).
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        let sel = Selection {
            albums: AlbumSelector::All {
                excluded: std::collections::BTreeSet::new(),
            },
            albums_explicit: true,
            smart_folders: SmartFolderSelector::All {
                include_sensitive: false,
                excluded: std::collections::BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries: LibrarySelector::default(),
            unfiled: false,
        };
        let states = commingle_test_states(2, &sel);
        let missing = find_multi_library_commingle_flags(
            &states,
            "%Y/%m/%d",
            "{library}/{album}",
            "{smart-folder}",
        );
        assert_eq!(
            missing,
            vec!["--folder-structure-smart-folders"],
            "only the active smart-folder pass with `{{library}}`-less template should be listed"
        );

        // Negative: same templates but smart-folder pass disabled. No
        // active pass lacks `{library}`, so the list is empty.
        let sel_no_smart = Selection {
            albums: AlbumSelector::All {
                excluded: std::collections::BTreeSet::new(),
            },
            albums_explicit: true,
            smart_folders: SmartFolderSelector::None,
            smart_folders_explicit: false,
            libraries: LibrarySelector::default(),
            unfiled: false,
        };
        let states_no_smart = commingle_test_states(2, &sel_no_smart);
        assert!(
            find_multi_library_commingle_flags(
                &states_no_smart,
                "%Y/%m/%d",
                "{library}/{album}",
                "{smart-folder}",
            )
            .is_empty(),
            "smart-folder inactive - its `{{library}}`-less template is irrelevant"
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_ignores_visibility_only_libraries() {
        use crate::commands::PassKind;
        let mut states = commingle_test_states(2, &selection_all_passes_active());
        states[1].plan = crate::commands::AlbumPlan { passes: Vec::new() };
        states[1].pass_scope = PassScope {
            include_albums: true,
            include_smart_folders: true,
            include_unfiled: true,
        };
        states[0].plan = crate::commands::AlbumPlan {
            passes: vec![make_pass("Album A", PassKind::Album)],
        };
        states[0].pass_scope = PassScope {
            include_albums: true,
            include_smart_folders: false,
            include_unfiled: false,
        };
        let missing =
            find_multi_library_commingle_flags(&states, "%Y/%m/%d", "{album}", "{smart-folder}");
        assert!(
            missing.is_empty(),
            "only one library has active passes - visibility-only zones must not trigger commingle warnings"
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_reports_all_missing_when_every_active_template_lacks_token()
     {
        let sel = selection_all_passes_active();
        let states = commingle_test_states(2, &sel);
        let missing =
            find_multi_library_commingle_flags(&states, "%Y/%m/%d", "{album}", "{smart-folder}");
        assert_eq!(
            missing,
            vec![
                "--folder-structure",
                "--folder-structure-albums",
                "--folder-structure-smart-folders",
            ],
            "every active template lacks `{{library}}` - all three should be listed",
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_reports_with_none_folder_structure_too() {
        // `none` (date hierarchy disabled) is the worst-case commingle:
        // every asset lands directly in the download dir. Still surfaces
        // the unfiled flag so the user knows the namespace is shared.
        let sel = selection_all_passes_active();
        let states = commingle_test_states(5, &sel);
        let missing =
            find_multi_library_commingle_flags(&states, "none", "{album}", "{smart-folder}");
        assert!(missing.contains(&"--folder-structure"));
    }

    #[test]
    fn find_multi_library_commingle_flags_short_circuits_when_no_passes_active() {
        // --album none + --smart-folder none + --unfiled false: every
        // pass is disabled, resolve_passes returns an empty plan, no
        // path is ever rendered, so multi-library can't commingle even
        // without `{library}` in any template.
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        let sel = Selection {
            albums: AlbumSelector::None,
            albums_explicit: true,
            smart_folders: SmartFolderSelector::None,
            smart_folders_explicit: false,
            libraries: LibrarySelector::default(),
            unfiled: false,
        };
        let states = commingle_test_states(3, &sel);
        assert!(
            find_multi_library_commingle_flags(&states, "%Y/%m/%d", "{album}", "{smart-folder}")
                .is_empty(),
            "no active passes - find_* must report empty"
        );
    }

    /// CG-6 (2026-05-03 test review): the existing suite asserts the
    /// *return value* of `find_multi_library_commingle_flags`, but
    /// nothing pins the contract that `warn_if_multi_library_paths_commingle`
    /// emits the `library_count` and `missing` lists as structured tracing
    /// fields. A future refactor that drops the named args (or replaces
    /// them with a positional message) would silently lose operator
    /// visibility into commingle scenarios. Pin the field shape so the
    /// regression is loud.
    #[test]
    fn warn_if_multi_library_paths_commingle_emits_structured_fields() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let sel = selection_all_passes_active();
        let states = commingle_test_states(3, &sel);
        warn_if_multi_library_paths_commingle(&states, "%Y/%m/%d", "{album}", "{smart-folder}");
        let events = capture.events();
        let warn = events
            .iter()
            .find(|event| {
                event.level == tracing::Level::WARN
                    && event
                        .message()
                        .is_some_and(|msg| msg.starts_with("Multi-library sync"))
            })
            .unwrap_or_else(|| panic!("missing commingle warning event: {events:?}"));
        assert_eq!(warn.field("library_count"), Some("3"));
        let missing = warn.field("missing").expect("missing field");
        for flag in [
            "--folder-structure",
            "--folder-structure-albums",
            "--folder-structure-smart-folders",
        ] {
            assert!(
                missing.contains(flag),
                "missing field should include {flag}, got {missing}"
            );
        }
    }

    /// CG-6 negative: when `{library}` is present in every active
    /// template, the warn must NOT fire. Catches the inverse mutation
    /// (warn fires unconditionally).
    #[tracing_test::traced_test]
    #[test]
    fn warn_if_multi_library_paths_commingle_silent_when_no_commingle() {
        let sel = selection_all_passes_active();
        let states = commingle_test_states(3, &sel);
        warn_if_multi_library_paths_commingle(
            &states,
            "{library}/%Y/%m/%d",
            "{library}/{album}",
            "{library}/{smart-folder}",
        );
        assert!(
            !logs_contain("library_count="),
            "warn must not fire when every active template carries `{{library}}`"
        );
    }

    // ── count_passes ────────────────────────────────────────────────────
    //
    // Pass tally feeds the per-library `Sync plan for library` info line.
    // The numbers map directly to API surface (one enumeration per album /
    // smart-folder pass + one for unfiled), so a regression that drops a
    // category would silently mislead the operator.

    fn make_pass(name: &str, kind: crate::commands::PassKind) -> crate::commands::AlbumPass {
        crate::commands::AlbumPass {
            kind,
            album: crate::icloud::photos::PhotoAlbum::stub_for_test(std::sync::Arc::from(name)),
            exclude_ids: std::sync::Arc::new(rustc_hash::FxHashSet::default()),
        }
    }

    fn make_incremental_album(zone_sync_token: &str) -> crate::icloud::photos::PhotoAlbum {
        use serde_json::json;
        crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(crate::test_helpers::MockPhotosSession::new().ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": zone_sync_token,
                    "moreComing": false,
                    "records": []
                }]
            }))),
        )
    }

    #[derive(Clone)]
    struct ConfigBridgeSession {
        zone: Arc<str>,
        inventory_token: Arc<str>,
        bridge_token: Arc<str>,
    }

    impl ConfigBridgeSession {
        fn new(zone: &str, inventory_token: &str, bridge_token: &str) -> Self {
            Self {
                zone: Arc::from(zone),
                inventory_token: Arc::from(inventory_token),
                bridge_token: Arc::from(bridge_token),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for ConfigBridgeSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(0));
            }
            if url.contains("/records/query?") {
                return Ok(serde_json::json!({
                    "records": [],
                    "syncToken": self.inventory_token.as_ref()
                }));
            }
            if url.contains("/changes/zone?") {
                return Ok(serde_json::json!({
                    "zones": [{
                        "zoneID": {"zoneName": self.zone.as_ref(), "ownerRecordName": "_defaultOwner"},
                        "syncToken": self.bridge_token.as_ref(),
                        "moreComing": false,
                        "records": []
                    }]
                }));
            }
            Ok(serde_json::json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone)]
    struct LegacyPendingDeleteSession {
        master_record_name: Arc<str>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for LegacyPendingDeleteSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/changes/zone?") {
                return Ok(serde_json::json!({
                    "zones": [{
                        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                        "syncToken": "zone-token-after-legacy-cleanup",
                        "moreComing": false,
                        "records": []
                    }]
                }));
            }
            if url.contains("/records/lookup?") {
                return Ok(serde_json::json!({
                    "records": [{
                        "recordName": self.master_record_name.as_ref(),
                        "serverErrorCode": "UNKNOWN_ITEM",
                        "reason": "record not found"
                    }]
                }));
            }
            Ok(serde_json::json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn make_one_photo_incremental_album_for_zone(
        zone: &str,
        zone_sync_token: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_one_photo_incremental_album_with_download(
            zone,
            zone_sync_token,
            "https://p01.icloud-content.com/photo.jpg",
            1024,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )
    }

    fn make_one_photo_incremental_album_with_download(
        zone: &str,
        zone_sync_token: &str,
        download_url: &str,
        size: u64,
        checksum: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        use serde_json::json;
        let page = full_album_page_with_download(
            zone,
            &format!("master-{zone}"),
            zone_sync_token,
            download_url,
            size,
            checksum,
        );
        let records = page
            .get("records")
            .expect("full album page records")
            .clone();

        make_full_album_with_session(
            zone,
            crate::test_helpers::MockPhotosSession::new().ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": zone, "ownerRecordName": "_defaultOwner"},
                    "syncToken": zone_sync_token,
                    "moreComing": false,
                    "records": records
                }]
            })),
        )
    }

    fn album_count_response(count: u64) -> serde_json::Value {
        serde_json::json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
        })
    }

    fn full_album_page(zone: &str, record_name: &str, sync_token: &str) -> serde_json::Value {
        full_album_page_with_download(
            zone,
            record_name,
            sync_token,
            "https://p01.icloud-content.com/photo.jpg",
            1024,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )
    }

    /// The `assetDate`/`addedDate` embedded by [`full_album_page_with_download`].
    const RUN_CYCLE_ASSET_DATE_MS: i64 = 1_700_000_000_000;

    /// The host-local `%Y/%m/%d` folder for an asset without an offset.
    fn run_cycle_expected_date_dir() -> String {
        chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS)
            .expect("valid asset timestamp")
            .with_timezone(&chrono::Local)
            .format("%Y/%m/%d")
            .to_string()
    }

    fn full_album_page_with_download(
        zone: &str,
        record_name: &str,
        sync_token: &str,
        download_url: &str,
        size: u64,
        checksum: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "records": [
                {
                    "recordName": record_name,
                    "recordType": "CPLMaster",
                    "fields": {
                        "filenameEnc": {"value": "cGhvdG8uanBn", "type": "STRING"},
                        "resOriginalRes": {
                            "value": {
                                "downloadURL": download_url,
                                "size": size,
                                "fileChecksum": checksum
                            }
                        },
                        "resOriginalWidth": {"value": 100, "type": "INT64"},
                        "resOriginalHeight": {"value": 100, "type": "INT64"},
                        "resOriginalFileType": {"value": "public.jpeg"},
                        "itemType": {"value": "public.jpeg"},
                        "adjustmentRenderType": {"value": 0, "type": "INT64"}
                    },
                    "recordChangeTag": "ct-master"
                },
                {
                    "recordName": format!("asset-{record_name}"),
                    "recordType": "CPLAsset",
                    "fields": {
                        "masterRef": {
                            "value": {"recordName": record_name, "zoneID": {"zoneName": zone}},
                            "type": "REFERENCE"
                        },
                        "assetDate": {"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"},
                        "addedDate": {"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"}
                    },
                    "recordChangeTag": "ct-asset"
                }
            ],
            "syncToken": sync_token
        })
    }

    fn make_full_album_with_session(
        zone: &str,
        session: crate::test_helpers::MockPhotosSession,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_named_full_album_with_boxed_session(zone, "TestAlbum", Box::new(session))
    }

    fn make_full_album_with_boxed_session(
        zone: &str,
        session: Box<dyn crate::icloud::photos::PhotosSession>,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_named_full_album_with_boxed_session(zone, "TestAlbum", session)
    }

    fn make_named_full_album_with_boxed_session(
        zone: &str,
        name: &str,
        session: Box<dyn crate::icloud::photos::PhotosSession>,
    ) -> crate::icloud::photos::PhotoAlbum {
        use serde_json::json;
        crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": zone})),
                retry_config: retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            session,
        )
    }

    fn make_named_empty_full_album(
        zone: &str,
        name: &str,
        zone_sync_token: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_named_full_album_with_boxed_session(
            zone,
            name,
            Box::new(
                crate::test_helpers::MockPhotosSession::new()
                    .ok(album_count_response(0))
                    .ok(serde_json::json!({"records": [], "syncToken": zone_sync_token})),
            ),
        )
    }

    fn make_empty_full_album(zone_sync_token: &str) -> crate::icloud::photos::PhotoAlbum {
        make_empty_full_album_for_zone("PrimarySync", zone_sync_token)
    }

    fn make_empty_full_album_for_zone(
        zone: &str,
        zone_sync_token: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_named_empty_full_album(zone, "TestAlbum", zone_sync_token)
    }

    fn make_one_photo_full_album_for_zone(
        zone: &str,
        zone_sync_token: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_full_album_with_session(
            zone,
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(full_album_page(
                    zone,
                    &format!("master-{zone}"),
                    zone_sync_token,
                )),
        )
    }

    fn make_run_cycle_library_state(
        zone: &str,
        sync_token_key: &str,
        zone_sync_token: &str,
    ) -> LibraryState {
        make_run_cycle_library_state_with_album(
            zone,
            sync_token_key,
            make_incremental_album(zone_sync_token),
        )
    }

    fn make_run_cycle_library_state_with_album(
        zone: &str,
        sync_token_key: &str,
        album: crate::icloud::photos::PhotoAlbum,
    ) -> LibraryState {
        make_run_cycle_library_state_with_passes(
            zone,
            sync_token_key,
            vec![crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Unfiled,
                album,
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }],
        )
    }

    fn make_run_cycle_library_state_with_passes(
        zone: &str,
        sync_token_key: &str,
        passes: Vec<crate::commands::AlbumPass>,
    ) -> LibraryState {
        LibraryState {
            library: crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(crate::test_helpers::MockPhotosSession::new()),
                zone,
            ),
            pass_scope: PassScope {
                include_albums: passes
                    .iter()
                    .any(|pass| pass.kind == crate::commands::PassKind::Album),
                include_smart_folders: passes
                    .iter()
                    .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder),
                include_unfiled: passes
                    .iter()
                    .any(|pass| pass.kind == crate::commands::PassKind::Unfiled),
            },
            zone_name: zone.to_string(),
            sync_token_key: sync_token_key.to_string(),
            plan: crate::commands::AlbumPlan { passes },
            plan_is_stale: false,
            plan_needs_refresh: false,
            cross_zone_libraries: Vec::new(),
        }
    }

    async fn make_shared_session_for_run_cycle() -> (tempfile::TempDir, auth::SharedSession) {
        let dir = tempfile::tempdir().expect("session tempdir");
        let session = auth::session::Session::new(
            dir.path(),
            "test@example.com",
            "https://example.com",
            None,
        )
        .await
        .expect("test session");
        (dir, Arc::new(tokio::sync::RwLock::new(session)))
    }

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    struct RunCycleDownloadConfigOptions {
        media: config::MediaSelection,
        per_pass_paths: bool,
        recent: Option<u32>,
        skip_created_before: Option<config::CreatedDateFilter>,
        skip_created_after: Option<config::CreatedDateFilter>,
        file_match_policy: Option<crate::types::FileMatchPolicy>,
        #[cfg(feature = "xmp")]
        xmp_sidecar: bool,
        /// Passes run through `buffer_unordered(pass_parallelism)`, which is
        /// capped by this, so it must exceed one for passes to overlap.
        concurrent_downloads: Option<usize>,
    }

    fn media_without_photo_downloads() -> config::MediaSelection {
        config::MediaSelection {
            photos: false,
            videos: true,
            live_photos: true,
        }
    }

    fn make_run_cycle_download_config_builder(
        download_dir: &std::path::Path,
        db: Arc<dyn download::DownloadStore>,
    ) -> impl Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
    + '_ {
        make_run_cycle_download_config_builder_with_options(
            download_dir,
            db,
            RunCycleDownloadConfigOptions::default(),
        )
    }

    fn make_run_cycle_download_config_builder_with_options(
        download_dir: &std::path::Path,
        db: Arc<dyn download::DownloadStore>,
        options: RunCycleDownloadConfigOptions,
    ) -> impl Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
    + '_ {
        move |sync_mode, exclude_asset_ids, asset_groupings, library| {
            let mut config = download::DownloadConfig::test_default();
            config.directory = Arc::from(download_dir);
            config.folder_structure = "%Y/%m/%d".to_string();
            config.folder_structure_albums = Arc::from("%Y/%m/%d");
            config.folder_structure_smart_folders = Arc::from("%Y/%m/%d");
            if options.per_pass_paths {
                config.folder_structure_albums = Arc::from("{album}");
            }
            if let Some(file_match_policy) = options.file_match_policy {
                config.file_match_policy = file_match_policy;
            }
            config.media = options.media;
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = options.xmp_sidecar;
            }
            config.recent = options.recent;
            config.skip_created_before = options.skip_created_before;
            config.skip_created_after = options.skip_created_after;
            if let Some(concurrent_downloads) = options.concurrent_downloads {
                config.concurrent_downloads = concurrent_downloads;
            }
            config.state_db = Some(Arc::clone(&db));
            config.sync_mode = sync_mode;
            config.exclude_asset_ids = exclude_asset_ids;
            config.asset_groupings = asset_groupings;
            config.library = library;
            Arc::new(config)
        }
    }

    fn make_recording_run_cycle_download_config_builder(
        download_dir: &std::path::Path,
        db: Arc<dyn download::DownloadStore>,
        observed_modes: Arc<std::sync::Mutex<Vec<download::SyncMode>>>,
    ) -> impl Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
    + '_ {
        let build_download_config = make_run_cycle_download_config_builder(download_dir, db);
        move |sync_mode, exclude_asset_ids, asset_groupings, library| {
            observed_modes
                .lock()
                .expect("recorded modes lock")
                .push(sync_mode.clone());
            build_download_config(sync_mode, exclude_asset_ids, asset_groupings, library)
        }
    }

    fn make_run_cycle_config() -> config::Config {
        let data_dir = tempfile::tempdir().expect("config data dir");
        let globals = config::GlobalArgs {
            username: Some("test@example.com".to_string()),
            domain: None,
            data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
        };
        config::Config::build(
            &globals,
            &cli::PasswordArgs::default(),
            cli::SyncArgs::default(),
            None,
        )
        .expect("test config")
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_cycle_capture_offset_drives_date_filter_path_and_sidecar() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use xmp_toolkit::{XmpMeta, xmp_ns};

        let server = crate::start_wiremock_or_skip!();
        let body = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        Mock::given(method("GET"))
            .and(path("/capture-offset.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let capture_date = chrono::NaiveDate::from_ymd_opt(2026, 2, 1).unwrap();
        let mut page = full_album_page_with_download(
            "PrimarySync",
            "capture-offset",
            "zone-token",
            &format!("{}/capture-offset.jpg", server.uri()),
            body.len() as u64,
            &checksum,
        );
        page["records"][1]["fields"]["assetDate"]["value"] =
            serde_json::json!(1_769_898_719_000_i64);
        page["records"][1]["fields"]["addedDate"]["value"] =
            serde_json::json!(1_769_898_719_000_i64);
        page["records"][1]["fields"]["timeZoneOffset"] =
            serde_json::json!({"value": 39_600, "type": "INT64"});

        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(page),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let boundary = config::CreatedDateFilter::CaptureDate(capture_date);
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                skip_created_before: Some(boundary),
                skip_created_after: Some(boundary),
                xmp_sidecar: true,
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let mut config = make_run_cycle_config();
        config.filters.skip_created_before = Some(boundary);
        config.filters.skip_created_after = Some(boundary);
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run capture-offset cycle");

        assert_eq!(result.stats.downloaded, 1);
        let downloaded = db.get_downloaded_page(0, 1).await.expect("downloaded row");
        let media_path = downloaded[0].local_path.as_ref().expect("downloaded path");
        assert!(
            media_path
                .parent()
                .is_some_and(|parent| parent.ends_with("2026/02/01"))
        );
        assert_eq!(std::fs::read(media_path).expect("downloaded media"), body);

        let sidecar = std::fs::read_to_string(media_path.with_file_name(format!(
            "{}.xmp",
            media_path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("media filename")
        )))
        .expect("capture-offset sidecar");
        let metadata = sidecar.parse::<XmpMeta>().expect("valid XMP sidecar");
        assert_eq!(
            metadata
                .property(xmp_ns::EXIF, "DateTimeOriginal")
                .expect("DateTimeOriginal")
                .value,
            "2026-02-01T09:31:59+11:00"
        );
        assert_eq!(
            metadata
                .property("http://cipa.jp/exif/1.0/", "OffsetTimeOriginal")
                .expect("OffsetTimeOriginal")
                .value,
            "+11:00"
        );
    }

    async fn run_full_cycle_with_album(
        album: crate::icloud::photos::PhotoAlbum,
        is_retry_failed: bool,
        controls: download::DownloadControls,
    ) -> CycleResult {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            is_retry_failed,
            &build_download_config,
            controls,
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle")
    }

    async fn run_empty_full_cycle(is_retry_failed: bool) -> CycleResult {
        run_empty_full_cycle_with_controls(
            is_retry_failed,
            download::DownloadControls::download_hidden(),
        )
        .await
    }

    async fn run_empty_full_cycle_with_controls(
        is_retry_failed: bool,
        controls: download::DownloadControls,
    ) -> CycleResult {
        run_full_cycle_with_album(
            make_empty_full_album("zone-tok-empty"),
            is_retry_failed,
            controls,
        )
        .await
    }

    async fn run_one_photo_full_cycle_with_controls(
        controls: download::DownloadControls,
    ) -> CycleResult {
        run_full_cycle_with_album(
            make_one_photo_full_album_for_zone("PrimarySync", "zone-tok-one"),
            false,
            controls,
        )
        .await
    }

    #[tokio::test]
    async fn run_cycle_recent_exact_inventory_stores_zone_token() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(40);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let session = crate::test_helpers::DynamicRecentPhotosSession::new(40)
            .with_filename_prefix("cycle-recent")
            .with_token("zone-tok-recent");
        let album = make_full_album_with_boxed_session("PrimarySync", Box::new(session));
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                recent: Some(40),
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run recent cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 40);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token"),
            Some("zone-tok-recent".to_owned()),
            "an N+1 probe that proves exact EOF may persist the zone token"
        );
        let events = capture.events();
        assert!(
            !events
                .iter()
                .any(|event| { event.field("reason") == Some("recent_limited_full_enumeration") }),
            "an exact recent inventory must not report truncation: {events:?}"
        );
        assert!(
            !events.iter().any(|event| {
                event.level == tracing::Level::WARN
                    && event.field("reason") == Some("recent_limited_full_enumeration")
            }),
            "recent-limited token suppression must not warn: {events:?}"
        );
    }

    #[tokio::test]
    async fn run_cycle_true_token_unsafe_condition_still_warns() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();

        let result = run_full_cycle_with_album(
            make_empty_full_album(""),
            false,
            download::DownloadControls::download_hidden(),
        )
        .await;

        assert_eq!(result.failed_count, 0);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some("icloud_blank_sync_token")
        );
        let events = capture.events();
        assert!(
            events.iter().any(|event| {
                event.level == tracing::Level::WARN
                    && event.field("diagnostic") == Some("icloud_blank_sync_token")
                    && event
                        .message()
                        .is_some_and(|message| message.contains("Provider checkpoint preserved"))
            }),
            "true token-unsafe sync-token suppression should still warn: {events:?}"
        );
    }

    #[tokio::test]
    async fn contract_malformed_required_asset_fields_block_checkpoint() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "stale-enum-config")
            .await
            .expect("seed stale enum config hash");

        let mut page = full_album_page("PrimarySync", "", "zone-tok-new");
        page["records"][0]["recordName"] = serde_json::json!("");
        page["records"][1]["recordName"] = serde_json::json!("");
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(page),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run malformed identity cycle");

        assert!(result.failed_count > 0);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(result.stats.assets_seen, 0);
        assert_eq!(result.stats.skipped.total(), 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "paired blank identities must replay from the prior checkpoint"
        );
    }

    #[tokio::test]
    async fn incremental_invalid_capture_date_preserves_prior_checkpoint() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .expect("seed current enum config hash");

        let mut page = full_album_page("PrimarySync", "bad-date", "zone-tok-new");
        page["records"][1]["fields"]["assetDate"]["value"] = serde_json::json!("not-a-timestamp");
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-tok-new",
                    "moreComing": false,
                    "records": page["records"].clone()
                }]
            })),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run malformed capture-date cycle");

        assert_eq!(result.failed_count, 0);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(crate::icloud::photos::asset::MALFORMED_REQUIRED_ASSET_FIELDS_REASON)
        );
        assert_eq!(result.stats.assets_seen, 0);
        assert_eq!(result.stats.skipped.total(), 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "invalid capture dates must replay from the prior checkpoint"
        );
    }

    #[tokio::test]
    async fn watch_recent_exact_first_cycle_seeds_incremental_token() {
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(20);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
            .with_filename_prefix("watch-recent")
            .with_token("zone-tok-watch");
        let album = make_full_album_with_boxed_session("PrimarySync", Box::new(session));
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                recent: Some(20),
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run first watch-like recent cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token"),
            Some("zone-tok-watch".to_owned())
        );
        let next_mode = determine_sync_mode(
            false,
            1,
            Some(db.as_ref()),
            "sync_token:PrimarySync",
            "PrimarySync",
        )
        .await;
        assert!(
            matches!(next_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "zone-tok-watch"),
            "a later watch cycle should use the token from proved recent EOF"
        );
    }

    #[tokio::test]
    async fn run_cycle_recent_exact_multiple_libraries_advance_each_zone() {
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(20);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let primary_session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
            .with_filename_prefix("primary-recent")
            .with_zone("PrimarySync")
            .with_token("zone-tok-primary");
        let shared_session_photos = crate::test_helpers::DynamicRecentPhotosSession::new(20)
            .with_filename_prefix("shared-recent")
            .with_zone("SharedSync-TEST")
            .with_token("zone-tok-shared");
        let primary_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_full_album_with_boxed_session("PrimarySync", Box::new(primary_session)),
        );
        let shared_state = make_run_cycle_library_state_with_album(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            make_full_album_with_boxed_session("SharedSync-TEST", Box::new(shared_session_photos)),
        );
        let states = vec![&primary_state, &shared_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                recent: Some(20),
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run recent multi-library cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 40);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read primary token"),
            Some("zone-tok-primary".to_owned())
        );
        assert_eq!(
            db.get_metadata("sync_token:SharedSync-TEST")
                .await
                .expect("read shared token"),
            Some("zone-tok-shared".to_owned())
        );
    }

    const ZERO_ASSET_WARNING_PREFIX: &str = "Sync completed after enumerating zero assets";

    #[test]
    fn count_passes_empty_plan_is_all_zero() {
        let plan = crate::commands::AlbumPlan { passes: Vec::new() };
        assert_eq!(count_passes(&plan), (0, 0, false));
    }

    #[test]
    fn count_passes_tallies_each_kind_independently() {
        use crate::commands::PassKind;
        let plan = crate::commands::AlbumPlan {
            passes: vec![
                make_pass("Vacation", PassKind::Album),
                make_pass("Family", PassKind::Album),
                make_pass("Favorites", PassKind::SmartFolder),
                make_pass("PrimarySync", PassKind::Unfiled),
            ],
        };
        assert_eq!(count_passes(&plan), (2, 1, true));
    }

    #[test]
    fn count_passes_unfiled_only_returns_zero_album_zero_smart_folder() {
        use crate::commands::PassKind;
        let plan = crate::commands::AlbumPlan {
            passes: vec![make_pass("PrimarySync", PassKind::Unfiled)],
        };
        assert_eq!(count_passes(&plan), (0, 0, true));
    }

    // ── should_retry_session_init ──────────────────────────────────────
    //
    // The init retry guard allows exactly one SRP re-auth on a session-error,
    // then bails. This prevents infinite loops under Docker restart policies.

    #[test]
    fn should_retry_session_init_true_on_first_421() {
        let err: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(should_retry_session_init(&err, false));
    }

    #[test]
    fn should_retry_session_init_false_on_second_421() {
        let err: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(!should_retry_session_init(&err, true));
    }

    #[test]
    fn should_retry_session_init_false_for_non_session_error() {
        let err: anyhow::Error =
            crate::icloud::error::ICloudError::Connection("timeout".into()).into();
        assert!(!should_retry_session_init(&err, false));
    }

    #[test]
    fn should_retry_session_init_true_for_first_401() {
        let err: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 401 }.into();
        assert!(should_retry_session_init(&err, false));
    }

    #[tokio::test]
    async fn clear_validation_cache_for_reauth_preserves_routing_state() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let username = "reauth@example.com";
        let session_path = auth::session_file_path(tempdir.path(), username);
        let session_json = br#"{
  "session_token": "tok_abc",
  "trust_token": "trust_xyz",
  "client_id": "client-1",
  "ckdatabasews_url": "https://old.example.test"
}"#;
        tokio::fs::write(&session_path, session_json)
            .await
            .expect("write session metadata");
        let cache_path = auth::validation_cache_file_path(tempdir.path(), username);
        tokio::fs::write(&cache_path, br#"{"validated_at":1}"#)
            .await
            .expect("write validation cache");

        clear_validation_cache_for_reauth(tempdir.path(), username).await;

        assert!(
            !cache_path.exists(),
            "stale validation cache must be removed before retrying auth"
        );
        let session_after = tokio::fs::read(&session_path)
            .await
            .expect("session metadata should remain readable");
        assert_eq!(
            session_after, session_json,
            "lenient recovery must preserve session_token so accountLogin can run before SRP"
        );
    }

    #[test]
    fn session_error_reauth_tries_persisted_session_before_stripping() {
        let source = include_str!("sync_loop.rs");
        let (_, tail) = source
            .split_once("async fn reauth_after_session_error")
            .expect("reauth helper should exist");
        let (body, _) = tail
            .split_once("async fn clear_validation_cache_for_reauth")
            .expect("next helper should delimit reauth helper body");

        let clear_cache = body
            .find("clear_validation_cache_for_reauth")
            .expect("session-error reauth must clear cache before retrying auth");
        let lenient_auth = body
            .find("match authenticate_sync_session(")
            .expect("session-error reauth must try persisted-session auth");
        let strip = body
            .find("auth::strip_session_routing_state")
            .expect("session-error reauth must keep forced-SRP fallback");

        assert!(
            clear_cache < lenient_auth && lenient_auth < strip,
            "session-error recovery must try accountLogin-capable auth before stripping session_token"
        );
    }

    #[test]
    fn take_pending_auth_returns_value_once() {
        let mut pending = Some("auth");

        assert_eq!(take_pending_auth(&mut pending).unwrap(), "auth");
        assert!(pending.is_none());
    }

    #[test]
    fn take_pending_auth_empty_state_returns_error() {
        let mut pending: Option<&str> = None;

        let err = take_pending_auth(&mut pending).unwrap_err();

        assert!(
            err.to_string()
                .contains("internal auth retry state missing before attempt"),
            "unexpected error: {err}"
        );
    }

    // ── determine_sync_mode ──────────────────────────────────────────
    //
    // Sync-mode decision is the gatekeeper for the kei "user data is sacred"
    // invariant: pick Full vs Incremental wrong and either (a) re-download
    // the world (waste) or (b) skip previously-failed assets (silent loss).
    // None of the four critical branches had a direct unit test before.

    fn make_state_db() -> Arc<dyn download::DownloadStore> {
        Arc::new(state::SqliteStateDb::open_in_memory().expect("open in-memory state DB"))
    }

    const SCOPED_DB_SYNC_TOKEN_FAILURE_KEY: &str = "scoped_db_sync_token";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MetadataSetFailure {
        Exact(&'static str),
        Prefix(&'static str),
    }

    impl MetadataSetFailure {
        fn matches(self, key: &str) -> bool {
            match self {
                Self::Exact(expected) => key == expected,
                Self::Prefix(prefix) => key.starts_with(prefix),
            }
        }
    }

    struct FailingMetadataSetDb {
        inner: Arc<dyn download::DownloadStore>,
        failure: MetadataSetFailure,
        get_failure: Option<MetadataSetFailure>,
        delete_prefix_failure: Option<&'static str>,
        message: &'static str,
        cancel_on_upsert: Option<CancellationToken>,
        replace_download_dir_on_upsert: Option<std::path::PathBuf>,
        fail_upsert_seen: bool,
        fail_mark_downloaded: bool,
        fail_refresh_downloaded_metadata: bool,
        /// Stands in for a concurrent pass: refreshes the row to this snapshot
        /// and marks it for rewrite in the window between the downloader
        /// writing the file and the finaliser deciding the marker's fate.
        refresh_on_mark_downloaded: Option<state::AssetMetadata>,
        /// Counts drains: `run_pending` starts each one with an offset-zero
        /// page fetch.
        drains: Arc<std::sync::atomic::AtomicUsize>,
        /// Fails the rewritten-media checksum write so a drain can be driven
        /// into the window where the file changed but the row has not caught up.
        fail_metadata_checksum_write: bool,
    }

    impl std::fmt::Debug for FailingMetadataSetDb {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FailingMetadataSetDb")
                .field("failure", &self.failure)
                .field("get_failure", &self.get_failure)
                .field("delete_prefix_failure", &self.delete_prefix_failure)
                .field("message", &self.message)
                .finish_non_exhaustive()
        }
    }

    impl FailingMetadataSetDb {
        fn new(
            inner: Arc<dyn download::DownloadStore>,
            failure: MetadataSetFailure,
            message: &'static str,
        ) -> Self {
            Self {
                inner,
                failure,
                get_failure: None,
                delete_prefix_failure: None,
                message,
                cancel_on_upsert: None,
                replace_download_dir_on_upsert: None,
                fail_upsert_seen: false,
                fail_mark_downloaded: false,
                fail_refresh_downloaded_metadata: false,
                refresh_on_mark_downloaded: None,
                drains: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail_metadata_checksum_write: false,
            }
        }

        #[cfg(feature = "xmp")]
        fn drain_counter(&self) -> Arc<std::sync::atomic::AtomicUsize> {
            Arc::clone(&self.drains)
        }

        fn without_set_failure(
            inner: Arc<dyn download::DownloadStore>,
            message: &'static str,
        ) -> Self {
            Self::new(
                inner,
                MetadataSetFailure::Exact("__unused_metadata_key__"),
                message,
            )
        }

        fn with_refresh_downloaded_metadata_failure(mut self) -> Self {
            self.fail_refresh_downloaded_metadata = true;
            self
        }

        fn with_get_failure(mut self, failure: MetadataSetFailure) -> Self {
            self.get_failure = Some(failure);
            self
        }

        fn with_cancel_on_upsert(mut self, token: CancellationToken) -> Self {
            self.cancel_on_upsert = Some(token);
            self
        }

        fn with_download_dir_replaced_on_upsert(mut self, path: std::path::PathBuf) -> Self {
            self.replace_download_dir_on_upsert = Some(path);
            self
        }

        fn with_mark_downloaded_failure(mut self) -> Self {
            self.fail_mark_downloaded = true;
            self
        }

        #[cfg(feature = "xmp")]
        fn with_refresh_on_mark_downloaded(mut self, metadata: state::AssetMetadata) -> Self {
            self.refresh_on_mark_downloaded = Some(metadata);
            self
        }

        fn with_upsert_seen_failure(mut self) -> Self {
            self.fail_upsert_seen = true;
            self
        }
    }

    #[async_trait::async_trait]
    impl state::DownloadStateStore for FailingMetadataSetDb {
        #[cfg(test)]
        async fn should_download(
            &self,
            library: &str,
            id: &str,
            version_size: &str,
            checksum: &str,
            local_path: &std::path::Path,
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .should_download(library, id, version_size, checksum, local_path)
                .await
        }

        async fn upsert_seen(
            &self,
            record: &state::types::AssetRecord,
        ) -> Result<(), state::error::StateError> {
            if self.fail_upsert_seen {
                return Err(state::error::StateError::LockPoisoned(self.message.into()));
            }
            let result = self.inner.upsert_seen(record).await;
            if result.is_ok() {
                if let Some(path) = &self.replace_download_dir_on_upsert {
                    let _ = std::fs::remove_dir_all(path);
                    std::fs::write(path, b"destination replaced by fault injection")
                        .expect("replace download dir with file");
                }
                if let Some(token) = &self.cancel_on_upsert {
                    token.cancel();
                }
            }
            result
        }

        async fn mark_downloaded(
            &self,
            library: &str,
            id: &str,
            version_size: &str,
            local_path: &std::path::Path,
            local_checksum: &str,
            download_checksum: Option<&str>,
        ) -> Result<(), state::error::StateError> {
            if self.fail_mark_downloaded {
                return Err(state::error::StateError::LockPoisoned(self.message.into()));
            }
            if let Some(newer) = &self.refresh_on_mark_downloaded {
                self.inner
                    .refresh_downloaded_asset_metadata(
                        library,
                        id,
                        newer,
                        true,
                        false,
                        state::METADATA_CAPTURE_REVISION,
                    )
                    .await?;
            }
            self.inner
                .mark_downloaded(
                    library,
                    id,
                    version_size,
                    local_path,
                    local_checksum,
                    download_checksum,
                )
                .await
        }

        async fn mark_failed(
            &self,
            library: &str,
            id: &str,
            version_size: &str,
            error: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .mark_failed(library, id, version_size, error)
                .await
        }

        async fn get_pending(
            &self,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_pending().await
        }

        async fn get_policy_excluded_ids_for_revalidation(
            &self,
            library: &str,
        ) -> Result<Vec<String>, state::error::StateError> {
            self.inner
                .get_policy_excluded_ids_for_revalidation(library)
                .await
        }

        async fn reset_failed(&self) -> Result<u64, state::error::StateError> {
            self.inner.reset_failed().await
        }

        async fn prepare_for_retry(
            &self,
            library: Option<&str>,
            error_retention: state::RetryErrorRetention,
        ) -> Result<(u64, u64, u64), state::error::StateError> {
            self.inner.prepare_for_retry(library, error_retention).await
        }

        async fn promote_pending_to_failed(
            &self,
            seen_since: i64,
        ) -> Result<u64, state::error::StateError> {
            self.inner.promote_pending_to_failed(seen_since).await
        }

        async fn get_downloaded_ids(
            &self,
        ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError>
        {
            self.inner.get_downloaded_ids().await
        }

        async fn get_soft_deleted_downloaded_ids(
            &self,
        ) -> Result<std::collections::HashSet<(String, String)>, state::error::StateError> {
            self.inner.get_soft_deleted_downloaded_ids().await
        }

        async fn get_all_known_ids(
            &self,
        ) -> Result<std::collections::HashSet<(String, String)>, state::error::StateError> {
            self.inner.get_all_known_ids().await
        }

        async fn get_downloaded_checksums(
            &self,
        ) -> Result<
            std::collections::HashMap<(String, String, String), String>,
            state::error::StateError,
        > {
            self.inner.get_downloaded_checksums().await
        }

        async fn get_downloaded_local_paths(
            &self,
        ) -> Result<
            std::collections::HashMap<(String, String, String), std::path::PathBuf>,
            state::error::StateError,
        > {
            self.inner.get_downloaded_local_paths().await
        }

        async fn get_attempt_counts(
            &self,
        ) -> Result<std::collections::HashMap<(String, String), u32>, state::error::StateError>
        {
            self.inner.get_attempt_counts().await
        }

        async fn touch_last_seen_many(
            &self,
            library: &str,
            asset_ids: &[&str],
        ) -> Result<(), state::error::StateError> {
            self.inner.touch_last_seen_many(library, asset_ids).await
        }

        async fn upsert_asset_master_mapping(
            &self,
            library: &str,
            asset_record_name: &str,
            master_record_name: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .upsert_asset_master_mapping(library, asset_record_name, master_record_name)
                .await
        }

        async fn get_master_record_name_for_asset(
            &self,
            library: &str,
            asset_record_name: &str,
        ) -> Result<Option<String>, state::error::StateError> {
            self.inner
                .get_master_record_name_for_asset(library, asset_record_name)
                .await
        }

        async fn get_asset_record_names_for_master(
            &self,
            library: &str,
            master_record_name: &str,
        ) -> Result<Vec<String>, state::error::StateError> {
            self.inner
                .get_asset_record_names_for_master(library, master_record_name)
                .await
        }

        async fn get_asset_master_mappings(
            &self,
        ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError>
        {
            self.inner.get_asset_master_mappings().await
        }

        async fn get_legacy_master_state_owners(
            &self,
        ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError>
        {
            self.inner.get_legacy_master_state_owners().await
        }

        async fn claim_legacy_master_state_owner(
            &self,
            library: &str,
            master_record_name: &str,
            asset_record_name: &str,
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .claim_legacy_master_state_owner(library, master_record_name, asset_record_name)
                .await
        }

        async fn backfill_asset_master_mappings_from_album_memberships(
            &self,
        ) -> Result<u64, state::error::StateError> {
            self.inner
                .backfill_asset_master_mappings_from_album_memberships()
                .await
        }

        async fn mark_policy_excluded(
            &self,
            library: &str,
            id: &str,
            version_size: &str,
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .mark_policy_excluded(library, id, version_size)
                .await
        }

        async fn mark_soft_deleted(
            &self,
            library: &str,
            asset_id: &str,
            deleted_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .mark_soft_deleted(library, asset_id, deleted_at)
                .await
        }

        async fn mark_hidden_at_source(
            &self,
            library: &str,
            asset_id: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner.mark_hidden_at_source(library, asset_id).await
        }
    }

    #[async_trait::async_trait]
    impl state::DownloadContextStateStore for FailingMetadataSetDb {
        async fn get_downloaded_file_records(
            &self,
        ) -> Result<Vec<state::DownloadedFileRecord>, state::error::StateError> {
            self.inner.get_downloaded_file_records().await
        }
    }

    #[async_trait::async_trait]
    impl state::TempFileOwnershipStore for FailingMetadataSetDb {
        async fn claim_temp_file(
            &self,
            path: &std::path::Path,
        ) -> Result<(), state::error::StateError> {
            self.inner.claim_temp_file(path).await
        }

        async fn get_owned_temp_files_before(
            &self,
            claimed_before: i64,
        ) -> Result<Vec<state::OwnedTempFile>, state::error::StateError> {
            self.inner.get_owned_temp_files_before(claimed_before).await
        }

        async fn retire_temp_files(
            &self,
            paths: &[std::path::PathBuf],
        ) -> Result<u64, state::error::StateError> {
            self.inner.retire_temp_files(paths).await
        }
    }

    #[async_trait::async_trait]
    impl state::ReportStateStore for FailingMetadataSetDb {
        async fn get_failed(
            &self,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_failed().await
        }

        async fn get_failed_sample(
            &self,
            limit: u32,
        ) -> Result<(Vec<state::types::AssetRecord>, u64), state::error::StateError> {
            self.inner.get_failed_sample(limit).await
        }

        async fn get_failed_page(
            &self,
            offset: u64,
            limit: u32,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_failed_page(offset, limit).await
        }

        async fn get_pending_page(
            &self,
            offset: u64,
            limit: u32,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_pending_page(offset, limit).await
        }

        async fn get_summary(&self) -> Result<state::types::SyncSummary, state::error::StateError> {
            self.inner.get_summary().await
        }

        async fn get_downloaded_page(
            &self,
            offset: u64,
            limit: u32,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_downloaded_page(offset, limit).await
        }

        async fn start_sync_run_at(
            &self,
            started_at: chrono::DateTime<chrono::Utc>,
        ) -> Result<i64, state::error::StateError> {
            self.inner.start_sync_run_at(started_at).await
        }

        async fn start_sync_run(&self) -> Result<i64, state::error::StateError> {
            self.inner.start_sync_run().await
        }

        async fn complete_sync_run(
            &self,
            run_id: i64,
            stats: &state::types::SyncRunStats,
        ) -> Result<(), state::error::StateError> {
            self.inner.complete_sync_run(run_id, stats).await
        }

        async fn promote_orphaned_sync_runs(&self) -> Result<u64, state::error::StateError> {
            self.inner.promote_orphaned_sync_runs().await
        }
    }

    #[async_trait::async_trait]
    impl state::SyncTokenStore for FailingMetadataSetDb {
        async fn get_metadata(
            &self,
            key: &str,
        ) -> Result<Option<String>, state::error::StateError> {
            if self.get_failure.is_some_and(|failure| failure.matches(key)) {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.get_metadata(key).await
            }
        }

        async fn set_metadata(
            &self,
            key: &str,
            value: &str,
        ) -> Result<(), state::error::StateError> {
            if self.failure.matches(key) {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.set_metadata(key, value).await
            }
        }

        async fn delete_metadata_by_prefix(
            &self,
            prefix: &str,
        ) -> Result<u64, state::error::StateError> {
            if self.delete_prefix_failure == Some(prefix) {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.delete_metadata_by_prefix(prefix).await
            }
        }

        async fn commit_checkpoint_transition(
            &self,
            transition: state::CheckpointTransition,
        ) -> Result<(), state::error::StateError> {
            if transition
                .metadata_updates
                .iter()
                .any(|(key, _)| self.failure.matches(key))
                || transition
                    .metadata_deletes
                    .iter()
                    .any(|key| self.delete_prefix_failure == Some(key.as_str()))
            {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.commit_checkpoint_transition(transition).await
            }
        }

        async fn get_scoped_db_sync_token(
            &self,
            provider: &str,
            account: &str,
            shape_version: i64,
            scope_hash: &str,
        ) -> Result<Option<state::ScopedDbSyncToken>, state::error::StateError> {
            if self
                .get_failure
                .is_some_and(|failure| failure.matches(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY))
            {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner
                    .get_scoped_db_sync_token(provider, account, shape_version, scope_hash)
                    .await
            }
        }

        async fn upsert_scoped_db_sync_token(
            &self,
            token: state::ScopedDbSyncToken,
        ) -> Result<(), state::error::StateError> {
            if self.failure.matches(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY) {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.upsert_scoped_db_sync_token(token).await
            }
        }

        async fn delete_scoped_db_sync_tokens(&self) -> Result<u64, state::error::StateError> {
            self.inner.delete_scoped_db_sync_tokens().await
        }

        async fn begin_enum_progress(&self, zone: &str) -> Result<(), state::error::StateError> {
            self.inner.begin_enum_progress(zone).await
        }

        async fn end_enum_progress(&self, zone: &str) -> Result<(), state::error::StateError> {
            self.inner.end_enum_progress(zone).await
        }

        async fn list_interrupted_enumerations(
            &self,
        ) -> Result<Vec<String>, state::error::StateError> {
            self.inner.list_interrupted_enumerations().await
        }
    }

    #[async_trait::async_trait]
    impl state::MembershipStore for FailingMetadataSetDb {
        async fn add_asset_album(
            &self,
            library: &str,
            asset_id: &str,
            album_name: &str,
            source: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .add_asset_album(library, asset_id, album_name, source)
                .await
        }

        async fn get_all_asset_albums(
            &self,
            library: &str,
        ) -> Result<Vec<(String, String)>, state::error::StateError> {
            self.inner.get_all_asset_albums(library).await
        }

        async fn get_all_asset_people(
            &self,
            library: &str,
        ) -> Result<Vec<(String, String)>, state::error::StateError> {
            self.inner.get_all_asset_people(library).await
        }

        async fn get_asset_groupings(
            &self,
            library: &str,
            asset_ids: &[&str],
        ) -> Result<state::db::AssetGroupingRows, state::error::StateError> {
            self.inner.get_asset_groupings(library, asset_ids).await
        }

        async fn upsert_album_container(
            &self,
            library: &str,
            container_id: &str,
            album_name: &str,
            pass_kind: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .upsert_album_container(library, container_id, album_name, pass_kind)
                .await
        }

        async fn mark_album_container_deleted(
            &self,
            library: &str,
            container_id: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .mark_album_container_deleted(library, container_id)
                .await
        }

        async fn start_album_membership_snapshot(
            &self,
            library: &str,
            container_id: &str,
            enum_config_hash: Option<&str>,
        ) -> Result<i64, state::error::StateError> {
            self.inner
                .start_album_membership_snapshot(library, container_id, enum_config_hash)
                .await
        }

        async fn add_album_membership_to_snapshot(
            &self,
            library: &str,
            container_id: &str,
            generation: i64,
            asset_record_name: &str,
            master_record_name: Option<&str>,
            source: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .add_album_membership_to_snapshot(
                    library,
                    container_id,
                    generation,
                    asset_record_name,
                    master_record_name,
                    source,
                )
                .await
        }

        async fn upsert_album_membership_delta(
            &self,
            library: &str,
            container_id: &str,
            asset_record_name: &str,
            master_record_name: Option<&str>,
            source: &str,
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .upsert_album_membership_delta(
                    library,
                    container_id,
                    asset_record_name,
                    master_record_name,
                    source,
                )
                .await
        }

        async fn mark_album_membership_deleted(
            &self,
            library: &str,
            container_id: &str,
            asset_record_name: &str,
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .mark_album_membership_deleted(library, container_id, asset_record_name)
                .await
        }

        async fn complete_album_membership_snapshot(
            &self,
            library: &str,
            container_id: &str,
            generation: i64,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .complete_album_membership_snapshot(library, container_id, generation)
                .await
        }

        async fn invalidate_album_membership_snapshot(
            &self,
            library: &str,
            container_id: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .invalidate_album_membership_snapshot(library, container_id)
                .await
        }

        async fn selected_album_containers_have_complete_snapshots(
            &self,
            library: &str,
            container_ids: &[&str],
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .selected_album_containers_have_complete_snapshots(library, container_ids)
                .await
        }

        async fn get_live_selected_album_memberships_for_asset(
            &self,
            library: &str,
            asset_record_name: &str,
            selected_container_ids: &[&str],
        ) -> Result<Vec<state::db::AlbumMembershipRecord>, state::error::StateError> {
            self.inner
                .get_live_selected_album_memberships_for_asset(
                    library,
                    asset_record_name,
                    selected_container_ids,
                )
                .await
        }
    }

    #[async_trait::async_trait]
    impl state::MetadataRewriteStore for FailingMetadataSetDb {
        async fn record_metadata_write_failure(
            &self,
            library: &str,
            asset_id: &str,
            version_size: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .record_metadata_write_failure(library, asset_id, version_size)
                .await
        }

        async fn refresh_downloaded_asset_metadata(
            &self,
            library: &str,
            asset_id: &str,
            metadata: &state::AssetMetadata,
            mark_for_rewrite: bool,
            mark_capture_repair: bool,
            capture_revision: i64,
        ) -> Result<usize, state::error::StateError> {
            if self.fail_refresh_downloaded_metadata {
                return Err(state::error::StateError::LockPoisoned(
                    self.message.to_string(),
                ));
            }
            self.inner
                .refresh_downloaded_asset_metadata(
                    library,
                    asset_id,
                    metadata,
                    mark_for_rewrite,
                    mark_capture_repair,
                    capture_revision,
                )
                .await
        }

        async fn get_downloaded_metadata_hashes(
            &self,
        ) -> Result<
            std::collections::HashMap<(String, String, String), String>,
            state::error::StateError,
        > {
            self.inner.get_downloaded_metadata_hashes().await
        }

        async fn get_metadata_retry_markers(
            &self,
        ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError>
        {
            self.inner.get_metadata_retry_markers().await
        }

        async fn get_pending_metadata_rewrites_page(
            &self,
            library_scope: Option<&[&str]>,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            if offset == 0 {
                self.drains
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner
                .get_pending_metadata_rewrites_page(library_scope, offset, limit)
                .await
        }

        async fn get_pending_metadata_rewrites_page_for_queue(
            &self,
            queue: state::db::MetadataRewriteQueue,
            library_scope: Option<&[&str]>,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<state::db::PendingMetadataRewrite>, state::error::StateError> {
            if offset == 0 {
                self.drains
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner
                .get_pending_metadata_rewrites_page_for_queue(queue, library_scope, offset, limit)
                .await
        }

        async fn record_capture_repair_prepared(
            &self,
            pending: &state::db::PendingMetadataRewrite,
            output_checksum: &str,
            output_size: u64,
        ) -> Result<Option<state::db::CaptureRepairReceipt>, state::error::StateError> {
            self.inner
                .record_capture_repair_prepared(pending, output_checksum, output_size)
                .await
        }

        async fn finish_metadata_rewrite(
            &self,
            pending: &state::db::PendingMetadataRewrite,
            selected_queue: state::db::MetadataRewriteQueue,
            local_checksum: Option<&str>,
            pre_rewrite_checksum: Option<&str>,
            completion: state::db::MetadataRewriteCompletion,
        ) -> Result<bool, state::error::StateError> {
            if self.fail_metadata_checksum_write {
                return Err(state::error::StateError::LockPoisoned(self.message.into()));
            }
            self.inner
                .finish_metadata_rewrite(
                    pending,
                    selected_queue,
                    local_checksum,
                    pre_rewrite_checksum,
                    completion,
                )
                .await
        }

        async fn has_downloaded_without_metadata_hash(
            &self,
        ) -> Result<bool, state::error::StateError> {
            self.inner.has_downloaded_without_metadata_hash().await
        }

        async fn begin_metadata_capture_revision(
            &self,
            library: &str,
            target_revision: i64,
        ) -> Result<state::MetadataCaptureStatus, state::error::StateError> {
            self.inner
                .begin_metadata_capture_revision(library, target_revision)
                .await
        }

        async fn get_metadata_capture_candidates(
            &self,
            library: &str,
            target_revision: i64,
            limit: usize,
        ) -> Result<Vec<state::MetadataCaptureCandidate>, state::error::StateError> {
            self.inner
                .get_metadata_capture_candidates(library, target_revision, limit)
                .await
        }

        async fn record_metadata_capture_failure(
            &self,
            library: &str,
            target_revision: i64,
            error: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .record_metadata_capture_failure(library, target_revision, error)
                .await
        }

        async fn complete_metadata_capture_revision(
            &self,
            library: &str,
            target_revision: i64,
        ) -> Result<state::MetadataCaptureStatus, state::error::StateError> {
            self.inner
                .complete_metadata_capture_revision(library, target_revision)
                .await
        }

        async fn has_metadata_capture_work(
            &self,
            libraries: &[&str],
            target_revision: i64,
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .has_metadata_capture_work(libraries, target_revision)
                .await
        }
    }

    /// Retry work is rehydrated independently from source enumeration, so a
    /// stored provider checkpoint remains usable during `--retry-failed`.
    #[tokio::test]
    async fn determine_sync_mode_retry_failed_with_token_returns_incremental() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        // Pre-populate a stored token so we can verify it is ignored.
        db.set_metadata(sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        let mode = determine_sync_mode(
            true, // is_retry_failed
            1,
            Some(db.as_ref()),
            sync_token_key,
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
            "retry-failed should keep source tracking incremental, got {mode:?}"
        );
    }

    /// Retry-failed must neither consume nor clear the stored provider token.
    #[tokio::test]
    async fn determine_sync_mode_retry_failed_does_not_consume_or_clear_stored_token() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        db.set_metadata(sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        let retry_mode =
            determine_sync_mode(true, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
        assert!(
            matches!(retry_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
            "retry-failed should use the stored token, got {retry_mode:?}"
        );

        let normal_mode =
            determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
        assert!(
            matches!(normal_mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
            "normal sync should still use the stored token after retry-failed, got {normal_mode:?}"
        );
    }

    #[tokio::test]
    async fn determine_sync_mode_empty_stored_token_falls_back_to_full() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        // Empty-string token is the malformed case — should be ignored.
        db.set_metadata(sync_token_key, "")
            .await
            .expect("set empty token");

        let mode =
            determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "empty stored token must yield Full, got {mode:?}"
        );

        // And a present, non-empty token must trigger Incremental — pin the
        // happy path here too so the empty-string check can't be dropped
        // without breaking both assertions.
        db.set_metadata(sync_token_key, "real-token")
            .await
            .expect("set real token");
        let mode =
            determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
        assert!(
            matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "real-token"),
            "non-empty token must yield Incremental with that token, got {mode:?}"
        );
    }

    /// When the state DB read fails, fall back to Full rather than
    /// propagating. The watch loop must keep going even if sqlite hiccups —
    /// silently biasing toward Incremental on errors would mask data loss.
    ///
    /// The inline `FailingDb` implements only `SyncTokenStore`, so any future
    /// reroute inside `determine_sync_mode` has to either stay inside the
    /// token role or fail to compile.
    #[tokio::test]
    async fn determine_sync_mode_state_db_error_falls_back_to_full() {
        // Minimal failing impl: only `get_metadata` is reachable from
        // `determine_sync_mode`; the narrow role trait keeps any silent
        // reroute from compiling against this stub.
        struct FailingDb;

        #[async_trait::async_trait]
        impl state::SyncTokenStore for FailingDb {
            async fn get_metadata(
                &self,
                _: &str,
            ) -> Result<Option<String>, state::error::StateError> {
                Err(state::error::StateError::LockPoisoned("simulated".into()))
            }

            async fn set_metadata(&self, _: &str, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }

            async fn delete_metadata_by_prefix(
                &self,
                _: &str,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }

            async fn begin_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }

            async fn end_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }

            async fn list_interrupted_enumerations(
                &self,
            ) -> Result<Vec<String>, state::error::StateError> {
                unimplemented!()
            }
        }

        let db = FailingDb;

        let mode =
            determine_sync_mode(false, 1, Some(&db), "sync_token:PrimarySync", "PrimarySync").await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "DB read error must fall back to Full, got {mode:?}"
        );
    }

    /// Sanity: `state_db = None` (e.g. legacy run with no state path) must
    /// still produce Full. The match-arm exists in production today; pin
    /// it so a future refactor that drops the `else` branch tells us.
    #[tokio::test]
    async fn determine_sync_mode_no_state_db_returns_full() {
        let mode = determine_sync_mode::<state::SqliteStateDb>(
            false,
            1,
            None,
            "sync_token:PrimarySync",
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "no state DB must yield Full, got {mode:?}"
        );
    }

    // ── check_changes_database ───────────────────────────────────────
    //
    // Watch-mode wakes the sync loop on a fixed interval. The first thing
    // each cycle does is hit the `changes/database` endpoint to ask Apple
    // "anything actually changed?" If we mis-classify the response we
    // either hammer Apple uselessly (no changes but proceeded) or silently
    // skip a real delta (changes pending but skipped). Pin every branch.

    /// Build a `LibraryState` that's just enough for `check_changes_database`.
    /// The `plan` and `library` fields are unused by that function, so an
    /// empty plan + a stub library is safe.
    fn make_library_state(zone: &str, sync_token_key: &str) -> LibraryState {
        let stub_session = Box::new(
            crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({"records": []})),
        );
        LibraryState {
            library: crate::icloud::photos::PhotoLibrary::new_stub(stub_session),
            pass_scope: PassScope {
                include_albums: false,
                include_smart_folders: false,
                include_unfiled: false,
            },
            zone_name: zone.to_string(),
            sync_token_key: sync_token_key.to_string(),
            plan: crate::commands::AlbumPlan { passes: Vec::new() },
            plan_is_stale: false,
            plan_needs_refresh: false,
            cross_zone_libraries: Vec::new(),
        }
    }

    fn assert_proceed_changed(precheck: &WatchPrecheck, expected_zone: &str, expected_db: &str) {
        let WatchPrecheck::Proceed {
            changed_zones: Some(zones),
            db_sync_token_after_success: Some(db_token),
        } = precheck
        else {
            panic!("expected changed-zone proceed, got {precheck:?}");
        };
        assert_eq!(zones.len(), 1);
        assert!(
            zones.contains(expected_zone),
            "missing zone {expected_zone}"
        );
        assert_eq!(db_token, expected_db);
    }

    fn test_precheck_scope_for_states(states: &[LibraryState], scope_id: &str) -> DbPrecheckScope {
        let mut zones: Vec<String> = states.iter().map(|s| s.zone_name.clone()).collect();
        zones.sort();
        let selected_zones_json = serde_json::to_string(&zones).expect("serialize zones");
        let scope_json = serde_json::to_string(&serde_json::json!({
            "test_scope": scope_id,
            "selected_zones": zones,
        }))
        .expect("serialize scope");
        DbPrecheckScope {
            provider: SCOPED_DB_SYNC_TOKEN_PROVIDER.to_string(),
            account: "test@example.com".to_string(),
            shape_version: SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION,
            scope_hash: scope_id.to_string(),
            selected_zones_json,
            scope_json,
        }
    }

    async fn seed_scoped_db_token(
        db: &dyn state::SyncTokenStore,
        scope: &DbPrecheckScope,
        token: &str,
    ) {
        db.upsert_scoped_db_sync_token(scope.to_state_row(token))
            .await
            .expect("seed scoped db token");
    }

    async fn read_scoped_db_token(
        db: &dyn state::SyncTokenStore,
        scope: &DbPrecheckScope,
    ) -> Option<state::ScopedDbSyncToken> {
        db.get_scoped_db_sync_token(
            &scope.provider,
            &scope.account,
            scope.shape_version,
            &scope.scope_hash,
        )
        .await
        .expect("read scoped db token")
    }

    async fn check_single_library_changes_database(
        db: Option<&dyn download::DownloadStore>,
        lib_state: &LibraryState,
        svc: &mut crate::icloud::photos::PhotosService,
        scope: &DbPrecheckScope,
    ) -> WatchPrecheck {
        check_changes_database(
            db.map(|db| db as &dyn state::SyncTokenStore),
            std::slice::from_ref(lib_state),
            svc,
            scope,
        )
        .await
    }

    /// `more_coming=true` with empty zones must NOT skip the cycle.
    /// Production logic: `if zones.is_empty() && !more_coming { skip }`.
    /// A regression that flipped the conjunction would silently skip every
    /// page-bearing wakeup -- silent loss of pending changes.
    #[tokio::test]
    async fn check_changes_database_more_coming_does_not_skip() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-2",
            "moreComing": true,
            "zones": []
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn download::DownloadStore> = make_state_db();
        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-more");
        seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
                .await;

        assert!(
            matches!(
                precheck,
                WatchPrecheck::Proceed {
                    changed_zones: None,
                    db_sync_token_after_success: Some(ref token)
                } if token == "db-tok-2"
            ),
            "more_coming=true must not skip the cycle (more pages pending)"
        );
        let stored = read_scoped_db_token(db.as_ref(), &scope)
            .await
            .expect("scoped token should remain present");
        assert_eq!(stored.token, "db-tok-prev");
    }

    /// Empty zones + `more_coming=false` still skip this watch cycle, but
    /// must not advance the scoped DB token.
    /// A suspicious empty page should self-heal on the next wakeup by
    /// rechecking from the last persisted token.
    #[tokio::test]
    async fn check_changes_database_empty_zones_skip_without_advancing_scoped_db_token() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-3",
            "moreComing": false,
            "zones": []
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn download::DownloadStore> = make_state_db();
        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-empty");
        seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
                .await;

        assert_eq!(
            precheck,
            WatchPrecheck::SkipAll,
            "empty zones + more_coming=false must skip the cycle"
        );
        let stored = read_scoped_db_token(db.as_ref(), &scope)
            .await
            .expect("scoped token should still be present");
        assert_eq!(stored.token, "db-tok-prev");
    }

    /// A non-empty zones list MUST NOT skip — even
    /// when more_coming=false. This is the real-work path; pinning it
    /// alongside the skip path catches a flipped branch in either
    /// direction.
    #[tokio::test]
    async fn check_changes_database_zone_changes_present_does_not_skip() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-4",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "ps-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn download::DownloadStore> = make_state_db();
        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let scope =
            test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-changed");
        seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
                .await;
        assert_proceed_changed(&precheck, "PrimarySync", "db-tok-4");
        let stored = read_scoped_db_token(db.as_ref(), &scope)
            .await
            .expect("scoped token should remain present");
        assert_eq!(stored.token, "db-tok-prev");
    }

    #[tokio::test]
    async fn check_changes_database_multi_library_runs_only_changed_selected_zone() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-shared",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "shared-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn download::DownloadStore> = make_state_db();

        let states = vec![
            make_library_state("PrimarySync", "sync_token:PrimarySync"),
            make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
        ];
        let scope = test_precheck_scope_for_states(&states, "scope-shared");
        seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

        let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc, &scope).await;
        assert_proceed_changed(&precheck, "SharedSync-ABCD", "db-tok-shared");
    }

    #[tokio::test]
    async fn check_changes_database_unselected_zone_change_skips_selected_libraries() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-unselected",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "shared-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn download::DownloadStore> = make_state_db();
        let states = vec![make_library_state("PrimarySync", "sync_token:PrimarySync")];
        let scope = test_precheck_scope_for_states(&states, "scope-unselected");
        seed_scoped_db_token(db.as_ref(), &scope, "db-tok-prev").await;

        let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc, &scope).await;
        assert_eq!(precheck, WatchPrecheck::SkipAll);
        let stored = read_scoped_db_token(db.as_ref(), &scope)
            .await
            .expect("token persisted");
        assert_eq!(stored.token, "db-tok-unselected");
    }

    /// No stored scoped DB token must not skip, but can capture a
    /// changes/database token before the cycle. The token is only persisted
    /// after the cycle completes cleanly, so concurrent changes remain safe.
    #[tokio::test]
    async fn check_changes_database_no_stored_token_bootstraps_without_skipping() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-token-bootstrap",
            "moreComing": false,
            "zones": [
                {
                    "zoneID": {"zoneName": "PrimarySync"},
                    "syncToken": "zone-token-bootstrap"
                }
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        // Empty DB - no scoped database pre-check row set.
        let db: Arc<dyn download::DownloadStore> = make_state_db();
        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let scope =
            test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-missing");

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
                .await;
        assert!(
            matches!(
                precheck,
                WatchPrecheck::Proceed {
                    changed_zones: None,
                    db_sync_token_after_success: Some(ref token)
                } if token == "db-token-bootstrap"
            ),
            "bootstrap token must be deferred until the cycle succeeds"
        );
    }

    #[tokio::test]
    async fn check_changes_database_scoped_token_read_failure_proceeds_without_precheck() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );
        let inner = make_state_db();
        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(inner, "simulated scoped-token read failure")
                .with_get_failure(MetadataSetFailure::Exact(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY)),
        );

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let scope =
            test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-read-failure");
        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
                .await;

        assert_eq!(
            precheck,
            WatchPrecheck::proceed_all(),
            "scoped token read failure should fall back to the safe full cycle path"
        );
    }

    #[tokio::test]
    async fn check_changes_database_legacy_db_token_without_scoped_row_does_not_skip() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );
        let db: Arc<dyn download::DownloadStore> = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed legacy zone token");
        db.set_metadata(DB_SYNC_TOKEN_KEY, "db-tok-prev")
            .await
            .expect("seed legacy db token");

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let scope = test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-new");
        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
                .await;

        assert_eq!(
            precheck,
            WatchPrecheck::proceed_all(),
            "legacy unscoped db tokens are not scoped proof"
        );
    }

    #[tokio::test]
    async fn check_changes_database_scope_hash_mismatch_does_not_skip() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );
        let db: Arc<dyn download::DownloadStore> = make_state_db();
        let states = vec![make_library_state("PrimarySync", "sync_token:PrimarySync")];
        let narrow_scope = test_precheck_scope_for_states(&states, "recent-500");
        let broad_scope = test_precheck_scope_for_states(&states, "recent-1000");
        seed_scoped_db_token(db.as_ref(), &narrow_scope, "db-tok-narrow").await;

        let precheck =
            check_changes_database(Some(db.as_ref()), &states, &mut svc, &broad_scope).await;

        assert_eq!(
            precheck,
            WatchPrecheck::proceed_all(),
            "Phase 1 must require exact scope hash match"
        );
    }

    #[tokio::test]
    async fn check_changes_database_corrupt_stored_scope_json_does_not_skip() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );
        let db: Arc<dyn download::DownloadStore> = make_state_db();
        let states = vec![make_library_state("PrimarySync", "sync_token:PrimarySync")];
        let scope = test_precheck_scope_for_states(&states, "corrupt-scope");
        db.upsert_scoped_db_sync_token(state::ScopedDbSyncToken {
            provider: scope.provider.clone(),
            account: scope.account.clone(),
            shape_version: scope.shape_version,
            scope_hash: scope.scope_hash.clone(),
            selected_zones_json: scope.selected_zones_json.clone(),
            scope_json: "{not valid json".to_string(),
            token: "db-tok-corrupt".to_string(),
        })
        .await
        .expect("seed corrupt scoped token row");

        let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc, &scope).await;

        assert_eq!(
            precheck,
            WatchPrecheck::proceed_all(),
            "corrupt stored scope JSON must fall back to enumeration"
        );
    }

    #[tokio::test]
    async fn refresh_needed_library_plans_filters_to_changed_zones() {
        let mut states = vec![
            make_library_state("PrimarySync", "sync_token:PrimarySync"),
            make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
        ];
        for state in &mut states {
            state.plan_needs_refresh = true;
        }
        let mut changed_zones = rustc_hash::FxHashSet::default();
        changed_zones.insert("PrimarySync".to_string());
        let selection = crate::selection::Selection {
            albums: crate::selection::AlbumSelector::None,
            albums_explicit: false,
            smart_folders: crate::selection::SmartFolderSelector::None,
            smart_folders_explicit: false,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };
        let collection_context = CollectionContext {
            collection_album_names: std::collections::BTreeSet::new(),
            selected_smart_folder_names: Vec::new(),
        };
        let mut failures = 0;

        refresh_needed_library_plans(
            &mut states,
            &selection,
            &collection_context,
            Some(&changed_zones),
            &mut failures,
        )
        .await;

        assert!(
            !states[0].plan_needs_refresh,
            "changed zone should refresh before syncing"
        );
        assert!(
            states[1].plan_needs_refresh,
            "unchanged zone must not refresh albums on this cycle"
        );
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn refresh_needed_library_plans_without_zone_filter_refreshes_every_stale_plan() {
        let mut states = vec![
            make_library_state("PrimarySync", "sync_token:PrimarySync"),
            make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
        ];
        for state in &mut states {
            state.plan_needs_refresh = true;
            state.plan_is_stale = true;
        }
        let selection = crate::selection::Selection {
            albums: crate::selection::AlbumSelector::None,
            albums_explicit: false,
            smart_folders: crate::selection::SmartFolderSelector::None,
            smart_folders_explicit: false,
            libraries: all_libraries(),
            unfiled: false,
        };
        let collection_context = CollectionContext {
            collection_album_names: std::collections::BTreeSet::new(),
            selected_smart_folder_names: Vec::new(),
        };
        let mut failures = 2;

        refresh_needed_library_plans(
            &mut states,
            &selection,
            &collection_context,
            None,
            &mut failures,
        )
        .await;

        assert!(
            states.iter().all(|state| !state.plan_needs_refresh),
            "every stale plan should refresh when no changed-zone precheck filtered the cycle"
        );
        assert!(
            states.iter().all(|state| !state.plan_is_stale),
            "successful refresh should clear stale-plan token gates"
        );
        assert_eq!(failures, 0);
    }

    /// A scoped-token write failure on the
    /// unselected-zone skip path must not break watch mode.
    #[tokio::test]
    async fn check_changes_database_unselected_zone_token_persist_failure_still_skips() {
        use serde_json::json;
        let inner = make_state_db();
        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let scope =
            test_precheck_scope_for_states(std::slice::from_ref(&lib_state), "scope-write-fail");
        seed_scoped_db_token(inner.as_ref(), &scope, "db-tok-prev").await;
        let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
            inner,
            MetadataSetFailure::Exact(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY),
            "simulated scoped db sync token write failure",
        ));

        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-bad-write",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "ss-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc, &scope)
                .await;
        assert_eq!(precheck, WatchPrecheck::SkipAll);
    }

    // ── preload_asset_groupings ──────────────────────────────────────
    //
    // `preload_asset_groupings` must be best-effort: a hiccup
    // loading people must NOT empty the albums map, and vice versa.
    // XMP-sidecar runs read this struct; biasing the entire grouping
    // empty would silently strip metadata from every downloaded photo.

    /// When `get_all_asset_albums` succeeds but
    /// `get_all_asset_people` fails, the result still includes albums.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn preload_asset_groupings_partial_people_failure_keeps_albums() {
        struct PartialDb {
            inner: Arc<dyn download::DownloadStore>,
        }

        #[async_trait::async_trait]
        impl state::MembershipStore for PartialDb {
            async fn add_asset_album(
                &self,
                library: &str,
                asset_id: &str,
                album_name: &str,
                source: &str,
            ) -> Result<(), state::error::StateError> {
                self.inner
                    .add_asset_album(library, asset_id, album_name, source)
                    .await
            }

            async fn get_all_asset_albums(
                &self,
                library: &str,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                self.inner.get_all_asset_albums(library).await
            }

            async fn get_all_asset_people(
                &self,
                _: &str,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                Err(state::error::StateError::LockPoisoned(
                    "simulated people-table read failure".into(),
                ))
            }
        }

        // Seed the inner DB with two album memberships across two assets,
        // so we can verify the surviving map is non-empty.
        let inner = make_state_db();
        inner
            .add_asset_album("PrimarySync", "ASSET_A", "Vacation", "icloud")
            .await
            .expect("add album A");
        inner
            .add_asset_album("PrimarySync", "ASSET_B", "Family", "icloud")
            .await
            .expect("add album B");

        let db = PartialDb { inner };

        let groupings = preload_asset_groupings(Some(&db), "PrimarySync").await;
        // Albums must survive intact.
        assert_eq!(
            groupings.albums.len(),
            2,
            "two assets with album memberships expected, got {}",
            groupings.albums.len()
        );
        assert!(groupings.albums.contains_key("ASSET_A"));
        assert!(groupings.albums.contains_key("ASSET_B"));
        // People map is empty (the read failed) — but the function still
        // returns Some groupings rather than panicking.
        assert!(
            groupings.people.is_empty(),
            "people map should be empty when its read failed; got {} entries",
            groupings.people.len()
        );
    }

    /// Companion: `state_db = None` returns an empty grouping struct.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn preload_asset_groupings_no_db_returns_empty() {
        let groupings = preload_asset_groupings::<state::SqliteStateDb>(None, "PrimarySync").await;
        assert!(groupings.albums.is_empty());
        assert!(groupings.people.is_empty());
    }

    // `should_store_sync_token` is the single decision gate protecting the
    // sync-token from being advanced after a partial sync or a dry run. Both
    // situations would lose change events on the next incremental cycle
    // ("user data is sacred"). The matrix below pins every (outcome, dry_run)
    // combination so a future refactor can't relax the contract without a
    // failing test.

    /// A partial download failure MUST NOT advance the stored sync
    /// token. Otherwise the next incremental sync would skip past the
    /// failed assets' change events and never retry them.
    #[test]
    fn contract_sync_token_advance_requires_clean_cycle_blocks_partial_failure() {
        let outcome = download::DownloadOutcome::PartialFailure { failed_count: 3 };
        assert!(
            !should_store_sync_token(&outcome, false),
            "PartialFailure must NOT advance the sync token, even outside dry-run"
        );
        // dry_run=true cannot rescue a partial failure either.
        assert!(
            !should_store_sync_token(&outcome, true),
            "PartialFailure + dry_run must still NOT advance the sync token"
        );
    }

    /// `SessionExpired` is also a non-success outcome and
    /// MUST NOT advance the token. The cycle aborts mid-stream; the captured
    /// token may only reflect a subset of the work.
    #[test]
    fn sync_loop_session_expired_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::SessionExpired {
            auth_error_count: 5,
        };
        assert!(!should_store_sync_token(&outcome, false));
        assert!(!should_store_sync_token(&outcome, true));
    }

    /// In `--dry-run`, even a fully-successful pass MUST NOT advance
    /// the token. Dry-run promises no DB writes that affect the next real
    /// sync; advancing the token would silently break the next incremental.
    #[test]
    fn sync_loop_dry_run_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::Success;
        assert!(
            !should_store_sync_token(&outcome, true),
            "dry_run must NOT advance the sync token even on Success"
        );
    }

    /// Positive control: only the (Success, dry_run=false) combination
    /// advances the token. Pinning this branch prevents a future refactor
    /// from accidentally inverting the predicate.
    #[test]
    fn sync_loop_full_success_outside_dry_run_advances_sync_token() {
        let outcome = download::DownloadOutcome::Success;
        assert!(
            should_store_sync_token(&outcome, false),
            "(Success, dry_run=false) is the ONLY combination that should advance the token"
        );
    }

    /// A library that consumed a stale plan from a prior failed
    /// `resolve_passes` MUST NOT advance its sync token even when its
    /// outcome is `Success`. A reused plan can route assets to the wrong
    /// pass; advancing the affected zone token would skip the change events
    /// that would surface the corrected membership on the next cycle.
    #[test]
    fn sync_loop_stale_plan_blocks_sync_token_advance_even_on_success() {
        let outcome = download::DownloadOutcome::Success;
        // Baseline: without a stale plan, Success advances the token.
        assert!(should_store_sync_token_for_cycle(&outcome, false, false));
        // With a stale plan: even Success must NOT advance the token.
        assert!(
            !should_store_sync_token_for_cycle(&outcome, false, true),
            "same-library stale-plan flag must veto token advancement on Success"
        );
    }

    /// Stale-plan companion: dry_run and PartialFailure already block; pinning
    /// the matrix so a future refactor can't silently change the AND/OR
    /// shape of the gate.
    #[test]
    fn sync_loop_stale_plan_combines_with_existing_gates() {
        let success = download::DownloadOutcome::Success;
        let partial = download::DownloadOutcome::PartialFailure { failed_count: 1 };

        // PartialFailure: blocked regardless of stale-plan flag.
        assert!(!should_store_sync_token_for_cycle(&partial, false, false));
        assert!(!should_store_sync_token_for_cycle(&partial, false, true));

        // Dry-run: blocked regardless of stale-plan flag.
        assert!(!should_store_sync_token_for_cycle(&success, true, false));
        assert!(!should_store_sync_token_for_cycle(&success, true, true));

        // Only (Success, dry_run=false, library_stale=false) advances.
        assert!(should_store_sync_token_for_cycle(&success, false, false));
        assert!(!should_store_sync_token_for_cycle(&success, false, true));
    }

    #[tokio::test]
    async fn run_cycle_clean_zone_advances_despite_other_stale_plan() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "primary-prev")
            .await
            .expect("seed primary token");
        db.set_metadata("sync_token:SharedSync-TEST", "shared-prev")
            .await
            .expect("seed shared token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let clean_state =
            make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "primary-new");
        let mut stale_state = make_run_cycle_library_state(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            "shared-new",
        );
        stale_state.plan_is_stale = true;
        let states = vec![&clean_state, &stale_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must wait for every selected zone to be clean"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read primary token")
                .as_deref(),
            Some("primary-new"),
            "unaffected clean zone should advance its own token"
        );
        assert_eq!(
            db.get_metadata("sync_token:SharedSync-TEST")
                .await
                .expect("read shared token")
                .as_deref(),
            Some("shared-prev"),
            "stale zone must keep its old token"
        );
    }

    #[tokio::test]
    async fn run_cycle_stale_plan_blocks_database_precheck_token() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let mut lib_state =
            make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "zone-tok-new");
        lib_state.plan_is_stale = true;
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must wait until stale plans stop suppressing zone tokens"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "stale plan must leave the old zone token in place for replay"
        );
        assert!(!config.runtime.dry_run);
    }

    #[tokio::test]
    async fn run_cycle_provider_metadata_write_failure_preserves_zone_checkpoint() {
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(10);
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "simulated provider metadata write failure",
            )
            .with_refresh_downloaded_metadata_failure(),
        );
        let page = run_cycle_favourited_asset_page();
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-tok-new",
                    "moreComing": false,
                    "records": page["records"].clone()
                }]
            })),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                recent: Some(10),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert!(result.failed_count > 0);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.failed, 0);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some("provider_metadata_state_write_failed")
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "failed provider metadata write must preserve the replay checkpoint"
        );
    }

    /// Seeds a downloaded asset whose stored metadata is not a favourite,
    /// with its media file on disk, ready for the provider to report a
    /// metadata-only edit against it.
    async fn seed_run_cycle_metadata_drift_asset(
        inner: &Arc<dyn download::DownloadStore>,
        download_dir: &std::path::Path,
    ) {
        use chrono::TimeZone as _;

        let media_dir = download_dir.join(run_cycle_expected_date_dir());
        std::fs::create_dir_all(&media_dir).expect("create media directory");
        let media_path = media_dir.join("photo.jpg");
        std::fs::write(&media_path, vec![0u8; 1024]).expect("seed media file");

        let mut stored_metadata = state::AssetMetadata {
            is_favorite: false,
            ..state::AssetMetadata::default()
        };
        stored_metadata.refresh_hash();
        let record = crate::test_helpers::TestAssetRecord::new("master-PrimarySync")
            .filename("photo.jpg")
            .checksum("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .created_at(
                chrono::Utc
                    .timestamp_millis_opt(RUN_CYCLE_ASSET_DATE_MS)
                    .single()
                    .expect("valid asset date"),
            )
            .size(1024)
            .metadata(stored_metadata)
            .build();
        inner.upsert_seen(&record).await.expect("seed state row");
        inner
            .mark_downloaded(
                "PrimarySync",
                "master-PrimarySync",
                "original",
                &media_path,
                "local-checksum",
                None,
            )
            .await
            .expect("mark downloaded");
    }

    /// The same asset as `seed_run_cycle_metadata_drift_asset`, but reported
    /// by the provider as a favourite, so its metadata drifts from the row.
    fn run_cycle_favourited_asset_page() -> serde_json::Value {
        let mut page = full_album_page_with_download(
            "PrimarySync",
            "master-PrimarySync",
            "zone-tok-new",
            "https://p01.icloud-content.com/photo.jpg",
            1024,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        );
        page["records"][1]["fields"]["isFavorite"] =
            serde_json::json!({"value": 1, "type": "INT64"});
        page
    }

    /// The same asset carrying a caption edit instead of a favourite, so two
    /// passes can deliver different provider snapshots of one asset and both
    /// drift from the stored row.
    #[cfg(feature = "xmp")]
    fn run_cycle_captioned_asset_page() -> serde_json::Value {
        let mut page = run_cycle_favourited_asset_page();
        page["records"][1]["fields"]["isFavorite"] =
            serde_json::json!({"value": 0, "type": "INT64"});
        page["records"][1]["fields"]["captionEnc"] =
            serde_json::json!({"value": "edited in another pass", "type": "STRING"});
        page
    }

    /// #707: the paired single-pass streaming path must gate the zone
    /// checkpoint on provider-metadata durability exactly as the collecting
    /// path does. Identical to the collecting regression above but without
    /// `recent`, which is what routes the cycle through the streaming
    /// producer rather than collecting incremental planning.
    #[tokio::test]
    async fn run_cycle_single_pass_metadata_refresh_failure_preserves_zone_checkpoint() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "simulated provider metadata write failure",
            )
            .with_refresh_downloaded_metadata_failure(),
        );
        let mut page = full_album_page_with_download(
            "PrimarySync",
            "master-PrimarySync",
            "zone-tok-new",
            "https://p01.icloud-content.com/photo.jpg",
            1024,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        );
        // A metadata-only edit: the provider now reports the asset as a
        // favourite while the stored row does not.
        page["records"][1]["fields"]["isFavorite"] =
            serde_json::json!({"value": 1, "type": "INT64"});
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-tok-new",
                    "moreComing": false,
                    "records": page["records"].clone()
                }]
            })),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.stats.downloaded, 0);
        assert!(
            result.stats.state_write_failures > 0,
            "the streaming producer must report the failed refresh as non-durable state"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "a failed provider metadata refresh must preserve the replay checkpoint"
        );
    }

    /// #707: the single-pass streaming path must advance the zone checkpoint
    /// once the refreshed provider metadata is durable, applying the edit
    /// without downloading media.
    #[tokio::test]
    async fn run_cycle_single_pass_metadata_refresh_advances_checkpoint_after_durable_state() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

        let db: Arc<dyn download::DownloadStore> =
            Arc::clone(&inner) as Arc<dyn download::DownloadStore>;
        let page = run_cycle_favourited_asset_page();
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-tok-new",
                    "moreComing": false,
                    "records": page["records"].clone()
                }]
            })),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(
            result.stats.downloaded, 0,
            "a metadata-only edit downloads nothing"
        );
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(
            inner.get_downloaded_page(0, 1).await.unwrap()[0]
                .metadata
                .is_favorite,
            "the edited provider metadata must reach the catalogue"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-new"),
            "durable provider state must allow the zone checkpoint to advance"
        );
    }

    #[tokio::test]
    async fn run_cycle_capture_revision_repair_advances_checkpoint_after_durable_metadata() {
        #[derive(Clone, Debug)]
        struct CaptureRevisionSession {
            records: Arc<Vec<serde_json::Value>>,
        }

        #[async_trait::async_trait]
        impl crate::icloud::photos::PhotosSession for CaptureRevisionSession {
            async fn post(
                &self,
                url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<serde_json::Value> {
                if url.contains("/records/lookup?") {
                    return Ok(serde_json::json!({"records": self.records.as_ref()}));
                }
                if url.contains("/changes/zone?") {
                    return Ok(serde_json::json!({
                        "zones": [{
                            "zoneID": {
                                "zoneName": "PrimarySync",
                                "ownerRecordName": "_defaultOwner"
                            },
                            "syncToken": "zone-tok-new",
                            "moreComing": false,
                            "records": []
                        }]
                    }));
                }
                Ok(serde_json::json!({"records": []}))
            }

            fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
                Box::new(self.clone())
            }
        }

        let config = make_run_cycle_config();
        let inner = Arc::new(state::SqliteStateDb::open_in_memory().expect("state db"));
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db = Arc::clone(&inner) as Arc<dyn download::DownloadStore>;
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&db, download_dir.path()).await;
        inner
            .upsert_asset_master_mapping(
                "PrimarySync",
                "asset-master-PrimarySync",
                "master-PrimarySync",
            )
            .await
            .expect("seed durable provider identity");
        assert!(
            inner
                .claim_legacy_master_state_owner(
                    "PrimarySync",
                    "master-PrimarySync",
                    "asset-master-PrimarySync",
                )
                .await
                .expect("seed legacy state owner")
        );
        inner.set_metadata_capture_revision_for_test("PrimarySync", "master-PrimarySync", 0);

        let page = run_cycle_favourited_asset_page();
        let records = page["records"]
            .as_array()
            .expect("provider page records")
            .clone();
        let album = make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(CaptureRevisionSession {
                records: Arc::new(records),
            }),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.metadata_capture_refreshed, 1);
        assert_eq!(result.stats.metadata_capture_remaining, 0);
        assert!(
            inner.get_downloaded_page(0, 1).await.unwrap()[0]
                .metadata
                .is_favorite,
            "automatic repair must durably store the provider metadata"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-new"),
            "durable capture repair must allow the zone checkpoint to advance"
        );
    }

    /// Runs one full-enumeration cycle with XMP sidecars enabled over a
    /// downloaded asset the provider now reports as a favourite.
    #[cfg(feature = "xmp")]
    async fn run_full_enumeration_metadata_cycle(
        inner: &Arc<dyn download::DownloadStore>,
        download_dir: &std::path::Path,
        per_pass_paths: bool,
        controls: download::DownloadControls,
    ) -> crate::sync_cycle::CycleResult {
        let config = make_run_cycle_config();
        let db: Arc<dyn download::DownloadStore> = Arc::clone(inner);
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(run_cycle_favourited_asset_page()),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir,
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                xmp_sidecar: true,
                per_pass_paths,
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            controls,
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle")
    }

    /// #707 review: both enumeration branches defer their queue, so a full
    /// enumeration drains once. A second drain in the same cycle would retry a
    /// failed rewrite immediately and report it twice.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_cycle_full_enumeration_reports_a_failed_rewrite_once() {
        for per_pass_paths in [false, true] {
            let inner = make_state_db();
            let download_dir = tempfile::tempdir().expect("download tempdir");
            seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;
            // A directory where the sidecar belongs makes the rewrite fail.
            std::fs::create_dir(
                download_dir
                    .path()
                    .join(run_cycle_expected_date_dir())
                    .join("photo.jpg.xmp"),
            )
            .expect("block the sidecar path");

            let result = run_full_enumeration_metadata_cycle(
                &inner,
                download_dir.path(),
                per_pass_paths,
                download::DownloadControls::download_hidden(),
            )
            .await;

            assert_eq!(
                result.stats.exif_failures, 1,
                "one failing rewrite must be attempted and reported once (per_pass_paths = {per_pass_paths})"
            );
            assert!(
                !inner
                    .get_pending_metadata_rewrites_page(None, 0, 10)
                    .await
                    .expect("read markers")
                    .is_empty(),
                "the marker must survive for the next run to retry"
            );
        }
    }

    /// #707 review: the interleaving the maintainer described. Two passes run
    /// concurrently over one asset carrying different provider snapshots. They
    /// must share one drain, so a single writer owns the file, and the sidecar
    /// must agree with whichever snapshot the row settled on with no marker
    /// left claiming otherwise.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_cycle_two_passes_over_one_asset_leave_the_sidecar_matching_the_row() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

        let counting = FailingMetadataSetDb::without_set_failure(
            Arc::clone(&inner),
            "__unused_metadata_message__",
        );
        let drains = counting.drain_counter();
        let db: Arc<dyn download::DownloadStore> = Arc::new(counting);
        let pass = |name: &str, page: serde_json::Value| crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: make_named_full_album_with_boxed_session(
                "PrimarySync",
                name,
                Box::new(
                    crate::test_helpers::MockPhotosSession::new()
                        .ok(album_count_response(1))
                        .ok(page),
                ),
            ),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        };
        let lib_state = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            vec![
                pass("Favourited", run_cycle_favourited_asset_page()),
                pass("Captioned", run_cycle_captioned_asset_page()),
            ],
        );
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                xmp_sidecar: true,
                per_pass_paths: true,
                concurrent_downloads: Some(2),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        let stored = inner
            .get_downloaded_page(0, 10)
            .await
            .expect("read downloaded rows");
        let row = stored.first().expect("the asset stays downloaded");
        let sidecar = std::fs::read_to_string(
            download_dir
                .path()
                .join(run_cycle_expected_date_dir())
                .join("photo.jpg.xmp"),
        )
        .expect("the drain writes a sidecar");

        assert_eq!(
            drains.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the passes must share one drain, so a single writer owns the file"
        );
        assert_eq!(
            sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
            row.metadata.rating == Some(5),
            "the sidecar must hold the snapshot the row settled on: {sidecar}"
        );
        assert!(
            inner
                .get_metadata_retry_markers()
                .await
                .expect("read markers")
                .is_empty(),
            "no marker may survive a completed drain"
        );
    }

    /// #707 review: a forwarded download writes the snapshot it was planned
    /// from. If a concurrent pass has since refreshed the row and raised a
    /// marker, the finaliser must not retire it, or the row keeps the new
    /// metadata while the file keeps the old and nothing is left to repair it.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_cycle_download_does_not_retire_a_marker_raised_for_newer_metadata() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let config = make_run_cycle_config();
        let inner = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

        // The row stays downloaded but its file is gone, so the media task is
        // forwarded rather than skipped as already on disk.
        let media_path = download_dir
            .path()
            .join(run_cycle_expected_date_dir())
            .join("photo.jpg");
        std::fs::remove_file(&media_path).expect("remove the media file");

        let mut newer = state::AssetMetadata {
            is_favorite: true,
            rating: Some(5),
            ..state::AssetMetadata::default()
        };
        newer.refresh_hash();
        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "__unused_metadata_message__",
            )
            .with_refresh_on_mark_downloaded(newer),
        );

        let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        Mock::given(method("GET"))
            .and(path("/photo.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&server)
            .await;

        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(full_album_page_with_download(
                    "PrimarySync",
                    "master-PrimarySync",
                    "zone-tok-new",
                    &format!("{}/photo.jpg", server.uri()),
                    body.len() as u64,
                    // The provider reports the stored version, so the media
                    // task is forwarded only because the file is missing. A
                    // different checksum would make this a new version and
                    // return the row to pending, which is a separate flow.
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                )),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                xmp_sidecar: true,
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        let row = inner
            .get_downloaded_page(0, 10)
            .await
            .expect("read downloaded rows")
            .into_iter()
            .next()
            .expect("the asset stays downloaded");
        assert_eq!(
            row.metadata.rating,
            Some(5),
            "precondition: the racing pass leaves the newer snapshot on the row"
        );

        let published = row.local_path.as_deref().expect("the download published");
        let mut sidecar_name = published.file_name().expect("a file name").to_os_string();
        sidecar_name.push(".xmp");
        let sidecar = std::fs::read_to_string(published.with_file_name(sidecar_name))
            .expect("sidecar written next to the media file");
        assert!(
            sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
            "the file must end on the snapshot the row holds: {sidecar}"
        );
    }

    /// #707 review: deferring the drain moved it outside the pipeline, which
    /// returns early for the read-only run modes. The drain writes sidecars,
    /// so it must stay behind the same gate.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_cycle_full_enumeration_writes_no_metadata_in_read_only_modes() {
        for controls in [
            download::DownloadControls::dry_run_hidden(),
            download::DownloadControls::new(
                download::DownloadRunMode::PrintFilenames,
                download::DownloadReporting::hidden(),
            ),
        ] {
            let inner = make_state_db();
            let download_dir = tempfile::tempdir().expect("download tempdir");
            seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;
            inner
                .record_metadata_write_failure("PrimarySync", "master-PrimarySync", "original")
                .await
                .expect("queue a rewrite");

            run_full_enumeration_metadata_cycle(&inner, download_dir.path(), false, controls).await;

            assert!(
                !download_dir
                    .path()
                    .join(run_cycle_expected_date_dir())
                    .join("photo.jpg.xmp")
                    .exists(),
                "a read-only run must not write a sidecar"
            );
            assert!(
                !inner
                    .get_pending_metadata_rewrites_page(None, 0, 10)
                    .await
                    .expect("read markers")
                    .is_empty(),
                "a read-only run must not retire the marker"
            );
        }
    }

    /// #707: the full-enumeration path shares the streaming producer with the
    /// single-pass path, so a failed provider-metadata refresh must equally
    /// stop the zone checkpoint from being stored.
    #[tokio::test]
    async fn run_cycle_full_enumeration_metadata_refresh_failure_preserves_zone_checkpoint() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        // No seeded zone token, so the cycle runs a full enumeration.
        let download_dir = tempfile::tempdir().expect("download tempdir");
        seed_run_cycle_metadata_drift_asset(&inner, download_dir.path()).await;

        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "simulated provider metadata write failure",
            )
            .with_refresh_downloaded_metadata_failure(),
        );
        let page = run_cycle_favourited_asset_page();
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(page),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::NoStoredToken),
            "this regression must exercise the full-enumeration path"
        );
        assert!(
            result.stats.state_write_failures > 0,
            "the full-enumeration producer must report the failed refresh as non-durable state"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token"),
            None,
            "a failed provider metadata refresh must not store a zone checkpoint"
        );
    }

    #[tokio::test]
    async fn run_cycle_zone_token_write_failure_blocks_database_precheck_token() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
            Arc::clone(&inner),
            MetadataSetFailure::Prefix(SYNC_TOKEN_PREFIX),
            "simulated sync-token write failure",
        ));
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state =
            make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "zone-tok-new");
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance after a zone-token write failure"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "failed zone-token write must leave old token in place for replay"
        );
    }

    #[tokio::test]
    async fn run_cycle_published_file_state_write_failure_blocks_token() {
        // A published file with a failed state write is unsafe to skip on the
        // next incremental cycle, even though the media bytes are on disk.
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "simulated mark_downloaded failure",
            )
            .with_mark_downloaded_failure(),
        );
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let master_record_name = "master-state-write-failure";
        let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        Mock::given(method("GET"))
            .and(path("/photo.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(full_album_page_with_download(
                    "PrimarySync",
                    master_record_name,
                    "zone-tok-after-state-write-failure",
                    &format!("{}/photo.jpg", server.uri()),
                    body.len() as u64,
                    &checksum,
                )),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        let final_dir = download_dir.path().join(run_cycle_expected_date_dir());
        let final_paths = std::fs::read_dir(&final_dir)
            .expect("final directory exists")
            .map(|entry| entry.expect("final file entry").path())
            .collect::<Vec<_>>();
        assert_eq!(
            final_paths.len(),
            1,
            "expected one final file: {final_paths:?}"
        );
        assert_eq!(
            std::fs::read(&final_paths[0]).expect("read local media file"),
            body,
            "media bytes must land before the state write fails"
        );
        assert_eq!(
            result.failed_count, 1,
            "state-write failure must make the cycle partial; stats: {:?}",
            result.stats
        );
        assert_eq!(result.stats.state_write_failures, 1);
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance after a state-write failure"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "failed mark_downloaded must leave the old zone token in place for replay"
        );
        assert!(
            inner
                .get_downloaded_page(0, 10)
                .await
                .expect("downloaded rows")
                .is_empty(),
            "asset must not be marked fully downloaded when mark_downloaded fails"
        );
    }

    #[tokio::test]
    async fn run_cycle_album_membership_failure_preserves_and_replays_checkpoint() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        Mock::given(method("GET"))
            .and(path("/photo.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let config = make_run_cycle_config();
        let inner = Arc::new(state::SqliteStateDb::open_in_memory().expect("state db"));
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let media_path = download_dir.path().join("TestAlbum/cGhvdG8uanBn.JPG");
        let page = full_album_page_with_download(
            "PrimarySync",
            "master-PrimarySync",
            "zone-tok-new",
            &format!("{}/photo.jpg", server.uri()),
            body.len() as u64,
            &checksum,
        );
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(page.clone())
                .ok(album_count_response(1))
                .ok(page.clone())
                .ok(album_count_response(1))
                .ok(page),
        );
        let lib_state = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            vec![crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Album,
                album,
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }],
        );
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                per_pass_paths: true,
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        inner.fail_asset_album_writes_for_test();
        let failed = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run failed grouping cycle");

        assert_eq!(failed.failed_count, 1, "cycle stats: {:?}", failed.stats);
        assert_eq!(
            failed.stats.state_write_failures, 1,
            "cycle stats: {:?}",
            failed.stats
        );
        assert_eq!(failed.stats.downloaded, 1);
        assert_eq!(
            std::fs::read(&media_path).expect("read landed media"),
            body,
            "the media must land even though grouping state is not durable"
        );
        assert!(!failed.db_sync_token_advance_safe);
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read preserved zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "a missing grouping relationship must preserve the replay checkpoint"
        );
        assert!(
            inner
                .get_all_asset_albums("PrimarySync")
                .await
                .expect("read missing grouping relationship")
                .is_empty()
        );

        inner.allow_asset_album_writes_for_test();
        for cycle in 2..=3 {
            let recovered = run_cycle(
                &states,
                &config,
                Some(db.as_ref()),
                false,
                &build_download_config,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("grouping replay cycle {cycle} failed: {error}"));

            assert_eq!(recovered.failed_count, 0);
            assert_eq!(recovered.stats.state_write_failures, 0);
            assert_eq!(
                recovered.stats.downloaded, 0,
                "cycle {cycle} must not redownload landed media"
            );
            assert_eq!(
                inner
                    .get_all_asset_albums("PrimarySync")
                    .await
                    .expect("read recovered grouping relationship"),
                vec![(
                    "asset-master-PrimarySync".to_string(),
                    "TestAlbum".to_string()
                )],
                "cycle {cycle} must leave one idempotent relationship"
            );
        }
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read recovered zone token")
                .as_deref(),
            Some("zone-tok-new"),
            "the checkpoint may advance after the grouping relationship is durable"
        );
        assert_eq!(
            std::fs::read(&media_path).expect("read stable media"),
            body,
            "grouping replay must not change landed media"
        );
    }

    #[tokio::test]
    async fn run_cycle_durable_expired_url_failure_advances_zone_checkpoint() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .expect(1)
            .mount(&server)
            .await;

        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let body = b"expired-body";
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        let album = make_one_photo_incremental_album_with_download(
            "PrimarySync",
            "zone-tok-next",
            &format!("{}/expired.jpg", server.uri()),
            body.len() as u64,
            &checksum,
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("expired URL cycle");

        assert_eq!(result.failed_count, 1);
        assert!(!result.stats.interrupted);
        assert_eq!(result.stats.state_write_failures, 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-next")
        );
        let summary = db.get_summary().await.expect("state summary");
        assert_eq!(summary.pending + summary.failed, 1);
    }

    #[tokio::test]
    async fn run_cycle_expired_url_without_durable_retry_preserves_zone_checkpoint() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .expect(1)
            .mount(&server)
            .await;

        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "simulated pending-row write failure",
            )
            .with_upsert_seen_failure(),
        );
        let body = b"expired-body";
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        let album = make_one_photo_incremental_album_with_download(
            "PrimarySync",
            "zone-tok-next",
            &format!("{}/expired.jpg", server.uri()),
            body.len() as u64,
            &checksum,
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("expired URL cycle with failed pending-row write");

        assert_eq!(result.failed_count, 2);
        assert_eq!(result.stats.state_write_failures, 1);
        assert!(!result.db_sync_token_advance_safe);
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "the provider checkpoint must not advance without a durable retry row"
        );
        let summary = inner.get_summary().await.expect("state summary");
        assert_eq!(summary.pending + summary.failed, 0);
    }

    #[tokio::test]
    async fn run_cycle_durable_retry_survives_checkpoint_commit_failure() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .and(path("/expired.jpg"))
            .respond_with(ResponseTemplate::new(410))
            .expect(1)
            .mount(&server)
            .await;

        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
            Arc::clone(&inner),
            MetadataSetFailure::Prefix(SYNC_TOKEN_PREFIX),
            "simulated checkpoint commit failure",
        ));
        let body = b"expired-body";
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        let album = make_one_photo_incremental_album_with_download(
            "PrimarySync",
            "zone-tok-next",
            &format!("{}/expired.jpg", server.uri()),
            body.len() as u64,
            &checksum,
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("expired URL cycle with checkpoint failure");

        assert_eq!(result.failed_count, 1);
        assert!(!result.db_sync_token_advance_safe);
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "a failed checkpoint commit must retain the replay token"
        );
        let summary = inner.get_summary().await.expect("state summary");
        assert_eq!(
            summary.pending + summary.failed,
            1,
            "durable retry work must survive a failed checkpoint commit"
        );
    }

    #[tokio::test]
    async fn run_cycle_config_hash_stage_failure_forces_full_without_replacing_checkpoint() {
        let config = make_run_cycle_config();
        let current_hash = download::compute_config_hash(&config);
        assert_ne!(current_hash, "old-hash");

        let inner = make_state_db();
        inner
            .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
            .await
            .expect("seed enum hash");
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
            Arc::clone(&inner),
            MetadataSetFailure::Exact(PENDING_ENUM_CONFIG_HASH_KEY),
            "simulated pending hash write failure",
        ));
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album("zone-tok-new"),
        );
        let states = vec![&lib_state];
        let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
        let build_download_config = make_recording_run_cycle_download_config_builder(
            download_dir.path(),
            Arc::clone(&db),
            Arc::clone(&observed_modes),
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::EnumConfigHashDrift)
        );
        let observed_modes = observed_modes.lock().expect("recorded modes lock").clone();
        assert!(
            observed_modes
                .iter()
                .all(|mode| matches!(mode, download::SyncMode::Full)),
            "config drift must not trust a surviving old incremental token in this cycle: {observed_modes:?}"
        );
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance until config-hash invalidation can persist safely"
        );
        assert_eq!(
            inner
                .get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read enum hash")
                .as_deref(),
            Some("old-hash"),
            "new hash must not become active when reconciliation could not be staged"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "failed reconciliation staging must preserve the last safe zone token"
        );
    }

    #[tokio::test]
    async fn run_cycle_enum_config_drift_atomically_promotes_bridged_checkpoint() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
            .await
            .expect("seed active enum hash");
        db.set_metadata("sync_token:PrimarySync", "prior-token")
            .await
            .expect("seed prior token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let album = make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "inventory-token",
                "bridge-token",
            )),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("inventory plus delta bridge should complete");

        assert!(result.db_sync_token_advance_safe);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("bridge-token")
        );
        let expected_hash = download::compute_config_hash(&config);
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(expected_hash.as_str())
        );
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn run_cycle_failed_token_repair_preserves_prior_sqlite_checkpoint() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "prior-token")
            .await
            .unwrap();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album(""),
        );
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("failed token repair should preserve the active state");

        assert!(!result.db_sync_token_advance_safe);
        assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
        assert_eq!(result.stats.same_cycle_recovery_successes, 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("prior-token")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("old-enum-hash")
        );
        assert!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn run_cycle_legacy_pending_delete_self_heals_without_full_inventory() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-token-before-legacy-cleanup")
            .await
            .expect("seed zone token");
        let master_record_name = "LEGACY_PENDING_MASTER";
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new(master_record_name)
                .filename("legacy-deleted.jpg")
                .build(),
        )
        .await
        .expect("seed legacy pending row");
        let album = make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(LegacyPendingDeleteSession {
                master_record_name: Arc::from(master_record_name),
            }),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let observed_modes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let build_download_config = make_recording_run_cycle_download_config_builder(
            download_dir.path(),
            Arc::clone(&db),
            Arc::clone(&observed_modes),
        );

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("legacy cleanup cycle");

        assert!(result.db_sync_token_advance_safe);
        assert!(
            observed_modes
                .lock()
                .expect("observed modes lock")
                .iter()
                .any(|mode| matches!(mode, download::SyncMode::Incremental { .. }))
        );
        assert!(result.stats.full_enumeration_reason.is_none());
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-token-after-legacy-cleanup")
        );
        let summary = db.get_summary().await.expect("state summary");
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.source_deleted, 1);
        assert_eq!(summary.awaiting_provider_verification, 0);
    }

    #[tokio::test]
    async fn run_cycle_multi_zone_reconciliation_preserves_all_active_tokens_on_partial_failure() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
            .await
            .unwrap();
        db.set_metadata("sync_token:PrimarySync", "primary-prior")
            .await
            .unwrap();
        db.set_metadata("sync_token:SharedSync-TEST", "shared-prior")
            .await
            .unwrap();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let primary = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_full_album_with_boxed_session(
                "PrimarySync",
                Box::new(ConfigBridgeSession::new(
                    "PrimarySync",
                    "primary-inventory",
                    "primary-bridge",
                )),
            ),
        );
        let shared = make_run_cycle_library_state_with_album(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            make_full_album_with_boxed_session(
                "SharedSync-TEST",
                Box::new(ConfigBridgeSession::new("SharedSync-TEST", "", "unused")),
            ),
        );
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &[&primary, &shared],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("partial multi-zone reconciliation should preserve active state");

        assert!(!result.db_sync_token_advance_safe);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("primary-prior")
        );
        assert_eq!(
            db.get_metadata("sync_token:SharedSync-TEST")
                .await
                .unwrap()
                .as_deref(),
            Some("shared-prior")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("old-enum-hash")
        );
        let expected_hash = download::compute_config_hash(&config);
        assert_eq!(
            db.get_metadata(&pending_zone_token_key(&expected_hash, "PrimarySync",))
                .await
                .unwrap()
                .as_deref(),
            Some("primary-bridge"),
            "the completed zone must retain its reconciliation checkpoint"
        );
        assert_eq!(
            db.get_metadata(&pending_zone_token_key(&expected_hash, "SharedSync-TEST",))
                .await
                .unwrap(),
            None
        );

        let primary_resume = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_full_album_with_boxed_session(
                "PrimarySync",
                Box::new(ConfigBridgeSession::new(
                    "PrimarySync",
                    "unused-inventory",
                    "primary-after-resume",
                )),
            ),
        );
        let shared_retry = make_run_cycle_library_state_with_album(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            make_full_album_with_boxed_session(
                "SharedSync-TEST",
                Box::new(ConfigBridgeSession::new(
                    "SharedSync-TEST",
                    "shared-inventory",
                    "shared-bridge",
                )),
            ),
        );

        let resumed = run_cycle(
            &[&primary_resume, &shared_retry],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("unfinished zone reconciliation should resume");

        assert!(resumed.db_sync_token_advance_safe);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("primary-after-resume")
        );
        assert_eq!(
            db.get_metadata("sync_token:SharedSync-TEST")
                .await
                .unwrap()
                .as_deref(),
            Some("shared-bridge")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(expected_hash.as_str())
        );
        assert_eq!(
            db.get_metadata(&pending_zone_token_key(&expected_hash, "PrimarySync",))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn run_cycle_multi_zone_status_preserves_an_earlier_checkpoint_hold() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let held = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album(""),
        );
        let advanced = make_run_cycle_library_state_with_album(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            make_empty_full_album("shared-token"),
        );
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &[&held, &advanced],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert!(!result.db_sync_token_advance_safe);
        assert_eq!(
            db.get_metadata("last_checkpoint_status")
                .await
                .expect("read checkpoint status")
                .as_deref(),
            Some("preserved")
        );
        assert_eq!(
            db.get_metadata("last_recovery_action")
                .await
                .expect("read recovery action")
                .as_deref(),
            Some("retry_passes")
        );
    }

    #[tokio::test]
    async fn run_cycle_download_config_hash_drift_keeps_source_incremental() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let old_download_dir = tempfile::tempdir().expect("old download tempdir");
        let new_download_dir = tempfile::tempdir().expect("new download tempdir");

        let old_build_download_config =
            make_run_cycle_download_config_builder(old_download_dir.path(), Arc::clone(&db));
        let old_download_config = old_build_download_config(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        );
        let old_hash = download::hash_download_config(&old_download_config);
        db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, &old_hash)
            .await
            .expect("seed old download hash");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "zone-tok-prev")
            .await
            .expect("seed zone token");

        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
            make_incremental_album("zone-tok-new"),
        );
        let states = vec![&lib_state];
        let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
        let build_download_config = make_recording_run_cycle_download_config_builder(
            new_download_dir.path(),
            Arc::clone(&db),
            Arc::clone(&observed_modes),
        );
        let new_hash = download::hash_download_config(&build_download_config(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        ));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.full_enumeration_reason, None);
        let observed_modes = observed_modes.lock().expect("recorded modes lock").clone();
        assert!(
            matches!(
                observed_modes.last(),
                Some(download::SyncMode::Incremental { zone_sync_token })
                    if zone_sync_token == "zone-tok-prev"
            ),
            "path-only drift must preserve incremental source tracking: {observed_modes:?}"
        );
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-new"),
            "the incremental source pass should refresh the selected zone token after success"
        );
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .expect("read active download hash")
                .as_deref(),
            Some(new_hash.as_str()),
            "an empty catalog completes local path reconciliation immediately"
        );
        assert_eq!(
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .expect("read pending download hash"),
            None
        );
    }

    #[tokio::test]
    async fn run_cycle_date_bound_expansion_preserves_existing_media_and_reaches_steady_state() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        #[derive(Clone, Debug)]
        struct DateBoundSession {
            records: Arc<Vec<serde_json::Value>>,
        }

        #[async_trait::async_trait]
        impl crate::icloud::photos::PhotosSession for DateBoundSession {
            async fn post(
                &self,
                url: &str,
                body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<serde_json::Value> {
                if url.contains("/internal/records/query/batch") {
                    return Ok(album_count_response((self.records.len() / 2) as u64));
                }
                if url.contains("/records/lookup?") {
                    return Ok(serde_json::json!({"records": self.records.as_ref()}));
                }
                if url.contains("/records/query?") {
                    let request: serde_json::Value = serde_json::from_str(&body)?;
                    let offset = request["query"]["filterBy"]
                        .as_array()
                        .and_then(|filters| {
                            filters.iter().find_map(|filter| {
                                (filter["fieldName"] == "startRank")
                                    .then(|| filter["fieldValue"]["value"].as_u64())
                                    .flatten()
                            })
                        })
                        .unwrap_or(0);
                    let limit = request["resultsLimit"].as_u64().unwrap_or(0) / 2;
                    let records: Vec<_> = self
                        .records
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .skip(offset as usize)
                        .take(limit as usize)
                        .flatten()
                        .cloned()
                        .collect();
                    return Ok(serde_json::json!({
                        "records": records,
                        "syncToken": "zone-token-inventory"
                    }));
                }
                if url.contains("/changes/zone?") {
                    return Ok(serde_json::json!({
                        "zones": [{
                            "zoneID": {
                                "zoneName": "PrimarySync",
                                "ownerRecordName": "_defaultOwner"
                            },
                            "syncToken": "zone-token-incremental",
                            "moreComing": false,
                            "records": []
                        }]
                    }));
                }
                Ok(serde_json::json!({"records": []}))
            }

            fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
                Box::new(self.clone())
            }
        }

        fn asset_records(
            record_name: &str,
            filename: &str,
            created: chrono::DateTime<chrono::Utc>,
            download_url: &str,
            body: &[u8],
        ) -> Vec<serde_json::Value> {
            let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
            let mut page = full_album_page_with_download(
                "PrimarySync",
                record_name,
                "unused",
                download_url,
                body.len() as u64,
                &checksum,
            );
            page["records"][0]["fields"]["filenameEnc"]["value"] =
                serde_json::json!(base64::engine::general_purpose::STANDARD.encode(filename));
            page["records"][1]["fields"]["assetDate"]["value"] =
                serde_json::json!(created.timestamp_millis());
            page["records"][1]["fields"]["addedDate"]["value"] =
                serde_json::json!(created.timestamp_millis());
            page["records"].as_array().expect("asset records").clone()
        }

        fn snapshot_files(
            root: &std::path::Path,
        ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
            fn visit(
                root: &std::path::Path,
                dir: &std::path::Path,
                files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
            ) {
                for entry in std::fs::read_dir(dir).expect("read media directory") {
                    let entry = entry.expect("read media entry");
                    let path = entry.path();
                    if path.is_dir() {
                        visit(root, &path, files);
                    } else {
                        files.insert(
                            path.strip_prefix(root)
                                .expect("relative media path")
                                .to_path_buf(),
                            std::fs::read(path).expect("read media file"),
                        );
                    }
                }
            }

            let mut files = std::collections::BTreeMap::new();
            visit(root, root, &mut files);
            files
        }

        let server = crate::start_wiremock_or_skip!();
        let january_body = &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        let april_body = &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x02, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        Mock::given(method("GET"))
            .and(path("/january.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(january_body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/april.jpg"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(april_body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let january_created = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let april_created = chrono::NaiveDate::from_ymd_opt(2026, 4, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        let mut records = asset_records(
            "APRIL",
            "april.jpg",
            april_created,
            &format!("{}/april.jpg", server.uri()),
            april_body,
        );
        records.extend(asset_records(
            "JANUARY",
            "january.jpg",
            january_created,
            &format!("{}/january.jpg", server.uri()),
            january_body,
        ));
        let album = make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(DateBoundSession {
                records: Arc::new(records),
            }),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let old_lower_bound = config::CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
        );
        let new_lower_bound = config::CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
        );
        let upper_bound = config::CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2026, 5, 31).unwrap(),
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let mut config = make_run_cycle_config();
        config.filters.skip_created_before = Some(old_lower_bound);
        config.filters.skip_created_after = Some(upper_bound);
        let old_enum_hash = download::compute_config_hash(&config);
        let old_builder = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                skip_created_before: Some(old_lower_bound),
                skip_created_after: Some(upper_bound),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let first_result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &old_builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run initial date-bound cycle");
        assert_eq!(
            first_result.stats.downloaded, 1,
            "{:#?}",
            first_result.stats
        );

        let first_snapshot = snapshot_files(download_dir.path());
        assert_eq!(first_snapshot.len(), 1);
        let april_record = db
            .get_downloaded_page(0, 10)
            .await
            .expect("read initial downloaded rows")
            .into_iter()
            .find(|record| record.id.as_ref() == "asset-APRIL")
            .expect("April downloaded row");
        let april_path = april_record.local_path.expect("April local path");
        assert_eq!(tokio::fs::read(&april_path).await.unwrap(), april_body);
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read initial enumeration hash")
                .as_deref(),
            Some(old_enum_hash.as_str())
        );
        db.set_metadata(
            &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
            "zone-token-old-bound",
        )
        .await
        .expect("seed prior provider checkpoint");
        let checkpoint_before_expansion = db
            .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read initial provider checkpoint");
        assert_eq!(
            checkpoint_before_expansion.as_deref(),
            Some("zone-token-old-bound")
        );

        let old_download_config = old_builder(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        );
        db.set_metadata(
            download::DOWNLOAD_CONFIG_HASH_KEY,
            &download::hash_legacy_download_config(&old_download_config),
        )
        .await
        .expect("seed pre-fix path hash");
        drop(old_builder);

        config.filters.skip_created_before = Some(new_lower_bound);
        let current_enum_hash = download::compute_config_hash(&config);
        let current_builder = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                skip_created_before: Some(new_lower_bound),
                skip_created_after: Some(upper_bound),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let current_download_config = current_builder(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        );
        let current_download_hash = download::hash_download_config(&current_download_config);
        assert_ne!(
            download::hash_legacy_download_config(&old_download_config),
            download::hash_legacy_download_config(&current_download_config)
        );

        let second_result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &current_builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run expanded date-bound cycle");
        assert_eq!(second_result.stats.downloaded, 1);
        assert_ne!(
            second_result.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::DownloadConfigHashDrift)
        );

        let second_snapshot = snapshot_files(download_dir.path());
        assert_eq!(second_snapshot.len(), 2);
        assert!(
            first_snapshot
                .iter()
                .all(|entry| second_snapshot.get(entry.0) == Some(entry.1))
        );
        assert_eq!(tokio::fs::read(&april_path).await.unwrap(), april_body);
        let downloaded = db
            .get_downloaded_page(0, 10)
            .await
            .expect("read expanded downloaded rows");
        assert_eq!(downloaded.len(), 2);
        assert_eq!(
            downloaded
                .iter()
                .find(|record| record.id.as_ref() == "asset-APRIL")
                .and_then(|record| record.local_path.as_deref()),
            Some(april_path.as_path())
        );
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .expect("read current path hash")
                .as_deref(),
            Some(current_download_hash.as_str())
        );
        assert_eq!(
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .expect("read pending path hash"),
            None
        );
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .expect("read provider checkpoint")
                .as_deref(),
            Some("zone-token-incremental")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read enumeration hash")
                .as_deref(),
            Some(current_enum_hash.as_str())
        );
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read pending enumeration hash"),
            None
        );

        let third_result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &current_builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run steady-state date-bound cycle");
        assert_eq!(third_result.stats.downloaded, 0);
        assert_eq!(snapshot_files(download_dir.path()), second_snapshot);
        assert_ne!(
            third_result.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::DownloadConfigHashDrift)
        );
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .expect("read steady path hash")
                .as_deref(),
            Some(current_download_hash.as_str())
        );
        assert_eq!(
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .expect("read steady pending path hash"),
            None
        );
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .expect("read steady provider checkpoint")
                .as_deref(),
            Some("zone-token-incremental")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read steady enumeration hash")
                .as_deref(),
            Some(current_enum_hash.as_str())
        );
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read steady pending enumeration hash"),
            None
        );
    }

    #[tokio::test]
    async fn run_cycle_multi_pass_persists_base_download_config_hash() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let passes = vec![
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Album,
                album: make_named_empty_full_album("PrimarySync", "Vacation", "zone-tok"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Album,
                album: make_named_empty_full_album("PrimarySync", "Family", "zone-tok"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Unfiled,
                album: make_named_empty_full_album("PrimarySync", "", "zone-tok"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
        ];
        let lib_state = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
            passes.clone(),
        );
        let options = RunCycleDownloadConfigOptions {
            per_pass_paths: true,
            ..RunCycleDownloadConfigOptions::default()
        };
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            options,
        );
        let base_config = build_download_config(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        );
        let base_hash = download::hash_download_config(&base_config);
        let pass_hashes: Vec<String> = passes
            .iter()
            .map(|pass| download::hash_download_config(&base_config.with_pass(pass)))
            .collect();

        let result = run_cycle(
            &[&lib_state],
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("multi-pass cycle should complete");

        assert_eq!(result.failed_count, 0);
        let stored_hash = db
            .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read stored download hash")
            .expect("download hash should be persisted");
        assert_eq!(
            stored_hash, base_hash,
            "global download config hash must be the base run-level hash"
        );
        assert!(
            pass_hashes.iter().take(2).all(|hash| hash != &stored_hash),
            "album-expanded folder templates must not overwrite the global hash: {pass_hashes:?}"
        );
    }

    #[tokio::test]
    async fn unchanged_multi_pass_second_cycle_is_not_download_config_hash_drift() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let options = RunCycleDownloadConfigOptions {
            per_pass_paths: true,
            ..RunCycleDownloadConfigOptions::default()
        };
        let first_state = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
            vec![
                crate::commands::AlbumPass {
                    kind: crate::commands::PassKind::Album,
                    album: make_named_empty_full_album("PrimarySync", "Vacation", "zone-tok-1"),
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                },
                crate::commands::AlbumPass {
                    kind: crate::commands::PassKind::Unfiled,
                    album: make_named_empty_full_album("PrimarySync", "", "zone-tok-1"),
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                },
            ],
        );
        let first_builder = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            options,
        );
        let first_result = run_cycle(
            &[&first_state],
            &config,
            Some(db.as_ref()),
            false,
            &first_builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("first multi-pass cycle should complete");
        assert_eq!(first_result.failed_count, 0);

        let second_state = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
            vec![crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Unfiled,
                album: make_empty_full_album("zone-tok-2"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }],
        );
        let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
        let base_builder = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            options,
        );
        let second_builder = {
            let observed_modes = Arc::clone(&observed_modes);
            move |sync_mode: download::SyncMode,
                  exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
                  asset_groupings: Arc<download::AssetGroupings>,
                  library: Arc<str>| {
                observed_modes
                    .lock()
                    .expect("recorded modes lock")
                    .push(sync_mode.clone());
                base_builder(sync_mode, exclude_asset_ids, asset_groupings, library)
            }
        };
        let second_result = run_cycle(
            &[&second_state],
            &config,
            Some(db.as_ref()),
            false,
            &second_builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("second cycle should complete");

        assert_ne!(
            second_result.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::DownloadConfigHashDrift),
            "pass-expanded hash from first run must not force path-drift reconciliation"
        );
        assert!(
            observed_modes
                .lock()
                .expect("recorded modes lock")
                .iter()
                .any(|mode| matches!(mode, download::SyncMode::Incremental { .. })),
            "unchanged config with a stored token should reach the incremental decision path"
        );
    }

    #[tokio::test]
    async fn run_cycle_interrupted_incremental_download_blocks_sync_token_advance() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let shutdown_token = CancellationToken::new();
        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(Arc::clone(&inner), "unused")
                .with_cancel_on_upsert(shutdown_token.clone()),
        );
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_one_photo_incremental_album_for_zone("PrimarySync", "zone-tok-new"),
        );
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &shutdown_token,
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert!(
            result.stats.interrupted,
            "cancellation before the download pass must be visible in cycle stats"
        );
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance after an interrupted download cycle"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "interrupted cycle must leave the old zone token in place for replay"
        );
        assert!(
            !download_dir
                .path()
                .join(run_cycle_expected_date_dir())
                .join("photo.jpg")
                .exists(),
            "test must not pass by completing the download before cancellation"
        );
    }

    #[tokio::test]
    async fn run_cycle_destination_replaced_after_enumeration_reports_partial_failure() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let config = make_run_cycle_config();
        let inner = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let download_root = download_dir.path().to_path_buf();
        let db: Arc<dyn download::DownloadStore> = Arc::new(
            FailingMetadataSetDb::without_set_failure(Arc::clone(&inner), "unused")
                .with_download_dir_replaced_on_upsert(download_root.clone()),
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let master_record_name = "master-mid-sync-destination-fault";
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(full_album_page_with_download(
                    "PrimarySync",
                    master_record_name,
                    "zone-tok-after-fault",
                    "https://p01.icloud-content.com/mid-sync-destination-unavailable.jpg",
                    8,
                    "AAAA",
                )),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");
        let events = capture.events();

        assert_eq!(
            result.failed_count, 1,
            "mid-sync destination loss must produce a failed sync result"
        );
        assert_eq!(result.stats.failed, 1);
        assert_eq!(result.stats.downloaded, 0);
        assert!(
            result.db_sync_token_advance_safe,
            "durably queued transfer failure must not replay the provider delta"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token"),
            Some("zone-tok-after-fault".to_string()),
            "source checkpoint may advance once failed transfer work is durable"
        );

        let failed = db.get_failed().await.expect("read failed assets");
        assert_eq!(failed.len(), 1, "failed asset should be persisted");
        let last_error = failed[0].last_error.as_deref().expect("failed asset error");
        assert!(
            last_error.contains("Could not open temporary download file")
                || last_error.contains("Could not create directory"),
            "failed asset error should name the failing filesystem operation, got: {last_error}"
        );
        let root = download_root.display().to_string();
        let asset_record_name = format!("asset-{master_record_name}");
        let failure_event = events
            .iter()
            .find(|event| {
                event.level == tracing::Level::ERROR
                    && event.message() == Some("Download failed")
                    && event.field("asset_id") == Some(asset_record_name.as_str())
            })
            .unwrap_or_else(|| panic!("missing structured download failure event: {events:?}"));
        assert!(
            failure_event
                .field("path")
                .is_some_and(|path| path.contains(&root)),
            "download failure event should include path under {root}, got {failure_event:?}"
        );

        if download_root.is_file() {
            std::fs::remove_file(&download_root).expect("remove injected download-root file");
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_full_zero_assets_warns_once() {
        let result = run_empty_full_cycle(false).await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::NoStoredToken)
        );
        assert!(
            logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "completed full sync with zero assets should be visible in normal logs"
        );
        assert!(
            logs_contain("library_count=1"),
            "zero-asset warning should carry structured library_count"
        );
        assert!(
            logs_contain("assets_seen=0"),
            "zero-asset warning should carry structured assets_seen"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_retry_failed_zero_assets_does_not_warn() {
        let result = run_empty_full_cycle(true).await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(download::FullEnumerationReason::NoStoredToken)
        );
        assert!(
            !logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "retry-failed no-op cycles must stay quiet"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_incremental_fallback_zero_assets_warns() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed sync token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album("zone-tok-empty"),
        );
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                per_pass_paths: true,
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "incremental requests that fall back to full enumeration must warn"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_warns_for_empty_library_when_another_library_has_assets() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let empty_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album_for_zone("PrimarySync", "zone-tok-empty"),
        );
        let nonempty_state = make_run_cycle_library_state_with_album(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            make_one_photo_full_album_for_zone("SharedSync-TEST", "zone-tok-one"),
        );
        let states = vec![&empty_state, &nonempty_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: config::MediaSelection {
                    photos: false,
                    videos: true,
                    live_photos: true,
                },
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 1);
        assert!(
            logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "empty library must warn even when the cycle-wide asset count is nonzero"
        );
        assert!(
            logs_contain("library=PrimarySync"),
            "warning must name the empty library"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_dry_run_nonempty_full_cycle_does_not_warn() {
        let result =
            run_one_photo_full_cycle_with_controls(download::DownloadControls::dry_run_hidden())
                .await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            !logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "dry-run asset scans must not warn just because assets_seen stays zero"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_print_filenames_nonempty_full_cycle_does_not_warn() {
        let controls = download::DownloadControls::new(
            download::DownloadRunMode::PrintFilenames,
            download::DownloadReporting::hidden(),
        );
        let result = run_one_photo_full_cycle_with_controls(controls).await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            !logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "print-only asset scans must not warn just because assets_seen stays zero"
        );
    }

    #[tokio::test]
    async fn offline_replay_full_pass_reaches_sync_loop_planning_boundary() {
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosFlow::new()
                .album_count(1)
                .query_photo_page("master-replay-sync-loop", Some("zone-tok-replay"))
                .empty_query_page(Some("zone-tok-replay"))
                .build(),
        );

        let result =
            run_full_cycle_with_album(album, false, download::DownloadControls::dry_run_hidden())
                .await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(
            result.stats.downloaded, 1,
            "dry-run sync-loop replay must reach download planning"
        );
        assert_eq!(result.stats.failed, 0);
    }

    // Periodic reconciliation cadence. The watch loop calls
    // `should_reconcile_this_cycle` once per cycle to decide whether to walk
    // the state DB and warn on missing local files. Tests pin the cadence
    // so a future refactor can't silently disable the schedule.

    /// When `every_n` is `None`, the predicate must NEVER fire — this
    /// is the default-disabled behaviour for daemons that don't opt into
    /// periodic reconciliation.
    #[test]
    fn periodic_reconcile_disabled_when_every_n_is_none() {
        for cycle in [1u64, 2, 24, 1_000, u64::MAX] {
            assert!(
                !should_reconcile_this_cycle(cycle, None),
                "cycle {cycle} with every_n=None must NOT trigger reconciliation"
            );
        }
    }

    /// `Some(0)` is treated identically to `None` — the config
    /// resolver also filters this case, but the predicate is the load-bearing
    /// gate so we pin both spellings here.
    #[test]
    fn periodic_reconcile_disabled_when_every_n_is_zero() {
        for cycle in [1u64, 2, 24, 1_000] {
            assert!(
                !should_reconcile_this_cycle(cycle, Some(0)),
                "cycle {cycle} with every_n=Some(0) must NOT trigger"
            );
        }
    }

    /// The first firing must be at cycle == every_n, NOT cycle 0 or
    /// cycle 1. A freshly-started daemon must run at least one full sync
    /// before burning startup time on a state-DB walk.
    #[test]
    fn periodic_reconcile_first_fires_at_cycle_n_not_at_cycle_zero() {
        // every_n = 24: cycles 1..23 must NOT fire; cycle 24 fires.
        for cycle in 1u64..24 {
            assert!(
                !should_reconcile_this_cycle(cycle, Some(24)),
                "cycle {cycle} must NOT trigger when every_n=24"
            );
        }
        assert!(
            should_reconcile_this_cycle(24, Some(24)),
            "cycle 24 with every_n=24 must trigger"
        );
        // Cycle 0 is the pre-loop sentinel and must never fire even when
        // 0 is divisible by N.
        assert!(
            !should_reconcile_this_cycle(0, Some(24)),
            "cycle 0 (pre-loop sentinel) must NEVER trigger"
        );
    }

    /// Subsequent firings must repeat at every multiple of `every_n`.
    /// Pinning a few cycles past the first firing guards against an
    /// off-by-one that lets the cadence drift over a long run.
    #[test]
    fn periodic_reconcile_fires_on_every_multiple_of_n() {
        let n = 24;
        for &cycle in &[24u64, 48, 72, 240, 24_000] {
            assert!(
                should_reconcile_this_cycle(cycle, Some(n)),
                "cycle {cycle} (multiple of {n}) must trigger"
            );
        }
        for &cycle in &[25u64, 47, 49, 71, 73, 239, 241] {
            assert!(
                !should_reconcile_this_cycle(cycle, Some(n)),
                "cycle {cycle} (NOT a multiple of {n}) must NOT trigger"
            );
        }
    }

    /// `every_n=1` makes every cycle trigger reconciliation. Allowed
    /// (chatty but not a bug) and pinned because users debugging a drift
    /// suspicion are likely to set it to 1 temporarily.
    #[test]
    fn periodic_reconcile_every_one_fires_every_cycle() {
        for cycle in 1u64..=10 {
            assert!(
                should_reconcile_this_cycle(cycle, Some(1)),
                "cycle {cycle} with every_n=1 must trigger"
            );
        }
        // Sentinel still excluded.
        assert!(!should_reconcile_this_cycle(0, Some(1)));
    }

    async fn seed_downloaded_for_local_drift_probe(
        db: &state::SqliteStateDb,
        id: &str,
        path: &std::path::Path,
        size: u64,
    ) {
        let record = crate::test_helpers::TestAssetRecord::new(id)
            .checksum(&format!("ck_{id}"))
            .filename(&format!("{id}.jpg"))
            .size(size)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            id,
            "original",
            path,
            &format!("ck_{id}"),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn bounded_local_drift_probe_marks_missing_file_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = state::SqliteStateDb::open_in_memory().unwrap();
        let missing_path = dir.path().join("missing.jpg");
        seed_downloaded_for_local_drift_probe(&db, "MISSING_PROBE", &missing_path, 100).await;

        let outcome = run_bounded_local_drift_probe(&db, 1).await;

        assert_eq!(outcome.scanned, 1);
        assert_eq!(outcome.drifted, 1);
        assert_eq!(outcome.marked_failed, 1);
        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(&*failed[0].id, "MISSING_PROBE");
        assert_eq!(
            failed[0].last_error.as_deref(),
            Some(crate::commands::reconcile::FILE_MISSING_REASON)
        );
    }

    #[tokio::test]
    async fn bounded_local_drift_probe_marks_truncated_file_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = state::SqliteStateDb::open_in_memory().unwrap();
        let path = dir.path().join("truncated.jpg");
        std::fs::write(&path, b"short").unwrap();
        seed_downloaded_for_local_drift_probe(&db, "TRUNCATED_PROBE", &path, 100).await;

        let outcome = run_bounded_local_drift_probe(&db, 1).await;

        assert_eq!(outcome.scanned, 1);
        assert_eq!(outcome.drifted, 1);
        assert_eq!(outcome.marked_failed, 1);
        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(&*failed[0].id, "TRUNCATED_PROBE");
        assert_eq!(
            failed[0].last_error.as_deref(),
            Some(crate::commands::reconcile::FILE_TRUNCATED_REASON)
        );
    }

    // `should_wait_for_2fa` decides whether the reauth-time 2FA
    // branch parks the loop on a code prompt or surfaces the error. In
    // one-shot mode there is no operator at the keyboard; the error MUST
    // bubble up so cron / systemd / CI exits non-zero.

    /// A 2FA-required error in one-shot (`is_watch_mode = false`)
    /// MUST NOT cause the helper to return `true`. The caller will then
    /// surface the error to the user instead of blocking forever on a
    /// 2FA prompt that no one is watching.
    #[test]
    fn run_sync_2fa_required_in_one_shot_returns_error() {
        let err: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        assert!(
            !should_wait_for_2fa(false, &err),
            "one-shot + 2FA-required must surface the error, not park on a prompt"
        );
    }

    /// In watch mode the same error is recoverable;
    /// the helper returns `true` so the loop can park.
    #[test]
    fn run_sync_2fa_required_in_watch_mode_waits() {
        let err: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        assert!(
            should_wait_for_2fa(true, &err),
            "watch + 2FA-required must wait on a code"
        );
    }

    /// Negative control: any non-2FA error MUST surface in both modes.
    /// Otherwise the loop could silently swallow a real failure (e.g.
    /// `FailedLogin`) by treating it as "wait for a code that won't come".
    #[test]
    fn run_sync_non_2fa_error_never_waits() {
        let err: anyhow::Error = auth::error::AuthError::FailedLogin("bad password".into()).into();
        assert!(
            !should_wait_for_2fa(false, &err),
            "one-shot + non-2FA must surface"
        );
        assert!(
            !should_wait_for_2fa(true, &err),
            "watch + non-2FA must surface; do not park on a code that will never arrive"
        );
    }

    /// Negative control: a non-AuthError (e.g. an arbitrary anyhow error
    /// with no downcast target) MUST NOT be treated as 2FA. Without this
    /// guard the predicate could mis-park on transport errors.
    #[test]
    fn run_sync_non_auth_error_never_waits() {
        let err: anyhow::Error = anyhow::anyhow!("network unreachable");
        assert!(!should_wait_for_2fa(false, &err));
        assert!(!should_wait_for_2fa(true, &err));
    }

    #[test]
    fn refresh_metadata_off_never_forces_one_shot() {
        // Not requested: no gating even under service mode or with filters.
        assert!(!refresh_metadata_forces_one_shot(false, true, true).unwrap());
    }

    #[test]
    fn refresh_metadata_forces_one_shot_for_plain_sync() {
        assert!(refresh_metadata_forces_one_shot(true, false, false).unwrap());
    }

    #[test]
    fn refresh_metadata_rejected_under_service_run() {
        let err = refresh_metadata_forces_one_shot(true, true, false).unwrap_err();
        assert!(err.to_string().contains("service run"), "{err}");
    }

    #[test]
    fn refresh_metadata_rejected_with_narrowing_filter() {
        let err = refresh_metadata_forces_one_shot(true, false, true).unwrap_err();
        assert!(err.to_string().contains("filters"), "{err}");
    }

    #[test]
    fn capture_timestamp_repair_requires_embedded_datetime_output() {
        let repair = download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
        let err = validate_capture_timestamp_repair(repair, &config::MetadataConfig::default())
            .unwrap_err();
        assert!(err.to_string().contains("set_exif_datetime"), "{err}");

        let metadata = config::MetadataConfig {
            set_exif_datetime: true,
            ..config::MetadataConfig::default()
        };
        assert_eq!(
            validate_capture_timestamp_repair(repair, &metadata).unwrap(),
            repair
        );
    }

    #[test]
    fn repair_truncated_off_never_forces_one_shot() {
        assert!(!repair_truncated_forces_one_shot(false, true).unwrap());
    }

    #[test]
    fn repair_truncated_forces_one_shot_for_plain_sync() {
        assert!(repair_truncated_forces_one_shot(true, false).unwrap());
    }

    #[test]
    fn repair_truncated_rejected_under_service_run() {
        let err = repair_truncated_forces_one_shot(true, true).unwrap_err();
        assert!(err.to_string().contains("service run"), "{err}");
    }

    #[test]
    fn refresh_tail_outcome_feeds_ledger_and_cycle_reporting() {
        let mut cycle_result = CycleResult {
            failed_count: 1,
            session_expired: false,
            stats: download::SyncStats {
                failed: 1,
                ..download::SyncStats::default()
            },
            db_sync_token_advance_safe: true,
        };

        merge_refresh_tail_outcome(&mut cycle_result, 2, false);

        assert_eq!(cycle_result.failed_count, 3);
        assert_eq!(cycle_result.stats.failed, 3);
        let ledger = sync_run_stats_from_cycle(&cycle_result);
        assert_eq!(ledger.assets_failed, 3);
        let facts = crate::cycle_reporter::CycleFacts::new(
            &cycle_result.stats,
            cycle_result.failed_count,
            cycle_result.session_expired,
            std::time::Duration::ZERO,
        );
        assert_eq!(facts.status, crate::cycle_reporter::CycleStatus::Failed);
    }

    // ── check_and_persist_enum_config_hash ─────────────────────────────

    #[tokio::test]
    async fn enum_config_hash_initial_persists_only() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-abc")
            .await
            .expect("set token");

        let outcome = check_and_persist_enum_config_hash(&db, "hash-1").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Initial);
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("hash-1"),
        );
        // First run must NOT clear pre-existing sync tokens.
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-abc"),
        );
    }

    #[tokio::test]
    async fn enum_config_hash_drift_stages_reconciliation_and_preserves_tokens() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
            .await
            .expect("seed old hash");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-primary")
            .await
            .expect("seed primary token");
        db.set_metadata(
            &format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"),
            "tok-shared",
        )
        .await
        .expect("seed shared token");

        let outcome = check_and_persist_enum_config_hash(&db, "new-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Changed);
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("old-hash"),
        );
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("new-hash")
        );
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-primary")
        );
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-shared")
        );
    }

    #[tokio::test]
    async fn enum_config_hash_stage_failure_keeps_old_hash_and_tokens() {
        let inner = make_state_db();
        inner
            .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
            .await
            .expect("seed enum hash");
        inner
            .set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "old-zone-token")
            .await
            .expect("seed zone token");
        let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
            Arc::clone(&inner),
            MetadataSetFailure::Exact(PENDING_ENUM_CONFIG_HASH_KEY),
            "simulated pending hash write failure",
        ));

        let outcome = check_and_persist_enum_config_hash(db.as_ref(), "new-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::ChangedTokenPurgeFailed);
        assert_eq!(
            inner
                .get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read enum hash")
                .as_deref(),
            Some("old-hash"),
            "new hash must not become active before reconciliation"
        );
        assert_eq!(
            inner
                .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .expect("read zone token")
                .as_deref(),
            Some("old-zone-token"),
            "the last safe token must survive a failed reconciliation-stage write"
        );
    }

    #[tokio::test]
    async fn enum_config_hash_unchanged_is_noop() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "stable-hash")
            .await
            .expect("seed stable hash");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
            .await
            .expect("seed token");

        let outcome = check_and_persist_enum_config_hash(&db, "stable-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Unchanged);
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-keep"),
        );
    }

    #[tokio::test]
    async fn enum_config_revert_clears_pending_reconciliation() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "active-hash")
            .await
            .unwrap();
        db.set_metadata(PENDING_ENUM_CONFIG_HASH_KEY, "abandoned-hash")
            .await
            .unwrap();
        db.set_metadata(
            &pending_zone_token_key("abandoned-hash", "PrimarySync"),
            "candidate-token",
        )
        .await
        .unwrap();

        let outcome = check_and_persist_enum_config_hash(&db, "active-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Unchanged);
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
            None
        );
        assert_eq!(
            db.get_metadata(&pending_zone_token_key("abandoned-hash", "PrimarySync"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn enum_config_new_drift_discards_superseded_zone_candidates() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "active-hash")
            .await
            .unwrap();
        db.set_metadata(PENDING_ENUM_CONFIG_HASH_KEY, "superseded-hash")
            .await
            .unwrap();
        db.set_metadata(
            &pending_zone_token_key("superseded-hash", "PrimarySync"),
            "candidate-token",
        )
        .await
        .unwrap();

        let outcome = check_and_persist_enum_config_hash(&db, "replacement-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Changed);
        assert_eq!(
            db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("replacement-hash")
        );
        assert_eq!(
            db.get_metadata(&pending_zone_token_key("superseded-hash", "PrimarySync"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn download_config_hash_drift_stages_reconciliation_without_clearing_token() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "old-download-hash")
            .await
            .expect("seed old path hash");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
            .await
            .expect("seed token");

        let outcome = check_download_config_hash_for_cycle(
            &db,
            "current-download-hash",
            "legacy-current-download-hash",
        )
        .await;

        assert_eq!(outcome, DownloadConfigHashOutcome::Changed);
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap(),
            Some("tok-keep".to_string()),
            "path drift must preserve the active provider cursor"
        );
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap(),
            Some("old-download-hash".to_string()),
            "the active path hash changes only after reconciliation"
        );
        assert_eq!(
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap(),
            Some("current-download-hash".to_string()),
            "the candidate path hash must be durable while reconciliation runs"
        );
    }

    #[tokio::test]
    async fn download_config_hash_initial_persists_current_hash() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
            .await
            .expect("seed token");

        let outcome = check_download_config_hash_for_cycle(
            &db,
            "current-download-hash",
            "legacy-current-download-hash",
        )
        .await;

        assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("current-download-hash"),
            "first observation should persist the stable run-level path hash"
        );
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-keep"),
            "initial hash persistence must not purge existing tokens"
        );
    }

    #[tokio::test]
    async fn download_config_revert_clears_pending_reconciliation() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "active-hash")
            .await
            .unwrap();
        db.set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, "abandoned-hash")
            .await
            .unwrap();

        let outcome =
            check_download_config_hash_for_cycle(&db, "active-hash", "legacy-active-hash").await;

        assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
        assert_eq!(
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn download_config_legacy_hash_migrates_without_reconciliation() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "legacy-current-hash")
            .await
            .unwrap();
        db.set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, "abandoned-hash")
            .await
            .unwrap();
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
            .await
            .unwrap();

        let outcome =
            check_download_config_hash_for_cycle(&db, "current-path-hash", "legacy-current-hash")
                .await;

        assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
        assert_eq!(
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("current-path-hash")
        );
        assert_eq!(
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-keep")
        );
    }

    /// When a previously-downloaded asset's local file exists but the
    /// asset is absent from the API response, kei must NOT delete the
    /// local file by default. This guards against a pagination bug
    /// being interpreted as mass remote deletion.
    #[tokio::test]
    async fn remote_deletion_local_file_preserved_per_default_keep_policy() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        let dir = tempfile::tempdir().unwrap();
        let local_path = dir.path().join("2025/06/15/deleted_asset.jpg");

        // Pre-seed: asset was previously downloaded
        tokio::fs::create_dir_all(local_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&local_path, b"old photo bytes")
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("DEL_ASSET")
            .checksum("old_ck")
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DEL_ASSET",
            "original",
            &local_path,
            "old_ck",
            None,
        )
        .await
        .unwrap();

        // Verify the asset is still marked as downloaded and the file
        // is recognized as present
        let should_dl = db
            .should_download(
                "PrimarySync",
                "DEL_ASSET",
                "original",
                "old_ck",
                &local_path,
            )
            .await
            .unwrap();
        assert!(!should_dl, "unchanged asset must not need download");
        assert!(
            local_path.exists(),
            "local file preserved per default policy"
        );
    }

    /// An empty local state must keep all summary and pending views
    /// internally consistent. This pins the read side that user-facing
    /// status/verify commands rely on before any remote assets have been
    /// observed.
    #[tokio::test]
    async fn empty_state_summary_has_no_pending_or_downloaded_rows() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 0, "empty DB must have zero assets");
        assert_eq!(summary.downloaded, 0, "empty DB must have zero downloaded");
        assert_eq!(summary.pending, 0, "empty DB must have zero pending");
        assert_eq!(summary.failed, 0, "empty DB must have zero failed");
        assert!(
            db.get_pending().await.unwrap().is_empty(),
            "fresh state must not surface phantom pending work"
        );
    }
}
