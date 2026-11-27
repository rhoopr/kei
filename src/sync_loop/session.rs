//! Session authentication, 2FA recovery, and idle lock handoffs.

use crate::commands::{
    MAX_REAUTH_ATTEMPTS, attempt_reauth, init_photos_service, resolve_libraries, wait_and_retry_2fa,
};
use crate::notifications::Notifier;
use crate::{auth, config, notifications};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncAuthErrorClass {
    TwoFactorRequired,
    LockContention,
    SessionReset,
    AccountChanged,
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
    } else if auth_err.is_account_identity_changed() {
        SyncAuthErrorClass::AccountChanged
    } else if auth_err.is_session_reset() {
        SyncAuthErrorClass::SessionReset
    } else {
        SyncAuthErrorClass::Other
    }
}

fn notify_two_factor_required(config: &config::Config, notifier: &Notifier) {
    let instructions = crate::cli::two_factor_instructions(config.runtime.config_path.as_deref());
    tracing::warn!(message = %instructions, "2FA required");
    notifier.notify(
        notifications::Event::TwoFaRequired,
        "Two-factor authentication required. Follow the instructions on the sync host.",
        &config.auth.username,
        None,
        None,
    );
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

fn take_scoped_pending_auth(
    pending: &mut Option<auth::AuthResult>,
    expected: Option<&str>,
) -> anyhow::Result<auth::AuthResult> {
    let mut result = take_pending_auth(pending)?;
    result.session.bind_principal(&result.data, expected)?;
    Ok(result)
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
    anyhow::ensure!(
        !config.runtime.dry_run,
        "Preview authentication expired; refresh authentication before retrying the preview"
    );
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

    clear_validation_cache_for_reauth(
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
    )
    .await;
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
            if !super::should_wait_for_2fa(is_watch_mode, &e) {
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
        config.auth.domain.as_str(),
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
            if !super::should_wait_for_2fa(is_watch_mode, &e) {
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

/// A provider rejection invalidates the cached validation, not durable sync state.
async fn prepare_mid_cycle_reauth(
    shared_session: &auth::SharedSession,
    config: &config::Config,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !config.runtime.dry_run,
        "Preview authentication expired; refresh authentication before retrying the preview"
    );
    clear_validation_cache_for_reauth(
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
    )
    .await;
    shared_session.write().await.reset_http_clients()?;
    Ok(())
}

async fn clear_validation_cache_for_reauth(
    cookie_dir: &std::path::Path,
    username: &str,
    realm: &str,
) {
    let cache_path = auth::validation_cache_file_path(cookie_dir, username, realm);
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
        if config.runtime.dry_run {
            auth::AuthPersistence::ReadOnly
        } else {
            auth::AuthPersistence::Writable
        },
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
    clear_validation_cache_for_reauth(
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
    )
    .await;
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
    notify_two_factor_required(config, notifier);
    wait_and_retry_2fa(
        &config.auth.cookie_directory,
        &config.auth.username,
        || authenticate_sync_session(config, password_provider, input_mode, expected_generation),
        config.auth.domain.as_str(),
    )
    .await
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

/// Re-acquire the lock after idle sleep, then re-validate the session.
pub(crate) async fn reacquire_session(
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
        if classify_sync_auth_error(&e) == SyncAuthErrorClass::AccountChanged {
            return Err(e);
        }
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

/// Authenticate startup, retaining the one-shot versus watch 2FA behavior.
pub(super) async fn authenticate_initial_session(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    input_mode: crate::InputMode,
) -> anyhow::Result<auth::AuthResult> {
    authenticate_before_discovery(config, password_provider, notifier, input_mode, None).await
}

async fn authenticate_before_discovery(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    input_mode: crate::InputMode,
    expected_generation: Option<auth::session::SessionGeneration>,
) -> anyhow::Result<auth::AuthResult> {
    match authenticate_sync_session(config, password_provider, input_mode, expected_generation)
        .await
    {
        Err(error)
            if !config.runtime.dry_run
                && super::should_wait_for_2fa(config.watch.interval.is_some(), &error) =>
        {
            let generation = auth::two_factor_generation(&error).or(expected_generation);
            notify_and_wait_for_2fa(config, password_provider, notifier, input_mode, generation)
                .await
        }
        result => result,
    }
}

/// Initialize Photos and selected libraries with one session-recovery retry.
pub(super) async fn initialize_libraries(
    mut auth_result: auth::AuthResult,
    api_retry_config: crate::retry::RetryConfig,
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    input_mode: crate::InputMode,
) -> anyhow::Result<(
    auth::SharedSession,
    crate::icloud::photos::PhotosService,
    Vec<crate::icloud::photos::PhotoLibrary>,
)> {
    let is_watch_mode = config.watch.interval.is_some();
    // CloudKit session/routing recovery: if init or the first CloudKit query
    // surfaces a session-error signature (401 stale session, or 421 persisting
    // after a pool reset), first retry the normal persisted-session auth path
    // before falling back to forced SRP. A second CloudKit failure bails cleanly
    // instead of looping under Docker's restart policy.
    auth_result
        .session
        .bind_principal(&auth_result.data, None)?;
    let expected_principal = auth_result.session.principal().map(str::to_owned);
    let mut pending_auth = Some(auth_result);
    let mut retried_after_session_error = false;
    let mut discovery_indexing = crate::icloud::photos::DiscoveryIndexing::default();
    let systemd = crate::systemd::SystemdNotifier::new(
        config.watch.notify_systemd && !config.runtime.dry_run,
    );
    loop {
        if is_watch_mode {
            systemd.notify_status("Discovering iCloud Photos libraries");
        }
        let this_auth = take_scoped_pending_auth(&mut pending_auth, expected_principal.as_deref())?;
        let released_generation = this_auth.session.generation();
        let init_result =
            init_photos_service(this_auth, api_retry_config, config.ui.personality_mode).await;
        let (error, live) = match init_result {
            Err(error) => (error, None),
            Ok((ss, mut ps)) => {
                let discovery = async {
                    let selected =
                        resolve_libraries(&config.filters.selection.libraries, &mut ps).await?;
                    // Planning needs the complete map. Qualify it before any
                    // state writes so failed discovery can be retried as a unit.
                    ps.all_libraries().await?;
                    Ok(selected)
                }
                .await;
                discovery_indexing.extend(ps.take_discovery_indexing());
                match discovery {
                    Ok(libraries) => {
                        ps.retain_discovery_indexing(&discovery_indexing, &libraries);
                        return Ok((ss, ps, libraries));
                    }
                    Err(error) => (error, Some((ss, ps))),
                }
            }
        };
        if should_retry_session_init(&error, retried_after_session_error) {
            tracing::warn!(
                %error,
                "CloudKit returned stale-session signature; retrying persisted-session authentication"
            );
            retried_after_session_error = true;
            pending_auth = Some(
                reauth_after_session_error(
                    config,
                    password_provider,
                    notifier,
                    live,
                    Some(released_generation),
                    is_watch_mode,
                    input_mode,
                )
                .await?,
            );
        } else {
            return Err(error);
        }
    }
}

/// Recover an expired cycle without charging user-action 2FA waits as failures.
pub(crate) async fn recover_expired_cycle(
    shared_session: &auth::SharedSession,
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    reauth_attempts: &mut u32,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let is_watch_mode = config.watch.interval.is_some();
    *reauth_attempts += 1;
    if *reauth_attempts >= MAX_REAUTH_ATTEMPTS {
        anyhow::bail!(
            "Your iCloud session expired and kei could not refresh it after {MAX_REAUTH_ATTEMPTS} attempts. Run `{}` and try again.",
            crate::cli::command_for_config("login", config.runtime.config_path.as_deref())
        );
    }
    tracing::warn!(
        reauth_attempts = *reauth_attempts,
        max_attempts = MAX_REAUTH_ATTEMPTS,
        "Session expired, attempting re-auth"
    );
    prepare_mid_cycle_reauth(shared_session, config).await?;
    match attempt_reauth(
        shared_session,
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
        password_provider,
        input_mode,
    )
    .await
    {
        Ok(()) => {
            tracing::info!("Re-auth successful, resuming download...");
            Ok(()) // The caller restarts the entire cycle.
        }
        Err(e) if classify_sync_auth_error(&e) == SyncAuthErrorClass::TwoFactorRequired => {
            // 2FA is user action, not a failed attempt -- don't
            // burn reauth_attempts so false wakeups from get-code
            // can't exhaust the limit.
            *reauth_attempts -= 1;

            notify_two_factor_required(config, notifier);
            if !super::should_wait_for_2fa(is_watch_mode, &e) {
                return Err(e);
            }

            wait_and_retry_2fa(
                &config.auth.cookie_directory,
                &config.auth.username,
                || {
                    attempt_reauth(
                        shared_session,
                        &config.auth.cookie_directory,
                        &config.auth.username,
                        config.auth.domain.as_str(),
                        password_provider,
                        input_mode,
                    )
                },
                config.auth.domain.as_str(),
            )
            .await?;
            Ok(())
        }
        Err(e) => {
            notifier.notify(
                notifications::Event::SessionExpired,
                &format!("Re-authentication failed: {e}"),
                &config.auth.username,
                None,
                None,
            );
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::auth;
    use crate::sync_loop::session::{
        SyncAuthErrorClass, classify_sync_auth_error, clear_validation_cache_for_reauth,
        is_session_error, prepare_mid_cycle_reauth, reacquire_session_lock_after_idle,
        should_retry_session_init, take_pending_auth,
    };
    use crate::sync_loop::should_wait_for_2fa;

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
        (dir, std::sync::Arc::new(tokio::sync::RwLock::new(session)))
    }

    fn make_run_cycle_config() -> crate::config::Config {
        let data_dir = tempfile::tempdir().expect("config data dir");
        let globals = crate::config::GlobalArgs {
            config_source: Default::default(),
            username: Some("test@example.com".to_string()),
            domain: None,
            data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
        };
        crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            crate::cli::SyncArgs::default(),
            None,
        )
        .expect("test config")
    }

    /// Open (or create) the account's sync state the way `kei sync` does.
    async fn core_state(
        directory: &std::path::Path,
        username: &str,
        owner: &crate::state::db::account::AccountOwner,
    ) -> crate::sync_core::State {
        let location = crate::sync_core::access::Location::resolve(directory, username, "com")
            .await
            .unwrap();
        match crate::sync_core::access::open(&location, owner, None)
            .await
            .unwrap()
        {
            Some(state) => state,
            None => crate::sync_core::access::create(&location, owner)
                .await
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn watch_principal_change_stops_before_state_work_and_same_principal_reopens() {
        let (directory, shared) = make_shared_session_for_run_cycle().await;
        let username = "test@example.com";
        let data: auth::AccountLoginResponse =
            serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-principal-a"}}))
                .unwrap();
        {
            let mut session = shared.write().await;
            session.bind_principal(&data, None).unwrap();
            session
                .session_data
                .insert("session_token".into(), "synthetic-session".into());
        }
        let owner =
            crate::state::db::account::AccountOwner::authenticated(username, "com", &data).unwrap();
        let db = core_state(directory.path(), username, &owner).await;
        db.set_kv("sync_token:PrimarySync", "retained-cursor")
            .await
            .unwrap();
        db.set_kv("pending_sync_token:epoch:PrimarySync", "retained-debt")
            .await
            .unwrap();
        let media = directory.path().join("historical.jpg");
        std::fs::write(&media, b"synthetic preserved media").unwrap();
        let mut config = make_run_cycle_config();
        config.auth.cookie_directory = directory.path().to_path_buf();
        let provider: crate::password::PasswordProvider =
            std::sync::Arc::new(|| panic!("fresh cache must avoid network/password lookup"));
        let cache = auth::validation_cache_file_path(directory.path(), username, "com");
        for dsid in ["synthetic-principal-a", "synthetic-principal-b", ""] {
            std::fs::write(&cache, serde_json::to_vec(&serde_json::json!({"validated_at":chrono::Utc::now().timestamp(),"account_data":{"dsInfo":{"dsid":dsid}}})).unwrap()).unwrap();
            let result =
                super::reacquire_session(&shared, &config, &provider, crate::InputMode::NoInput)
                    .await;
            if dsid == "synthetic-principal-a" {
                result.unwrap();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .downcast_ref::<auth::error::AuthError>()
                        .is_some_and(auth::error::AuthError::is_account_identity_changed)
                );
            }
            assert_eq!(
                db.get_kv("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("retained-cursor")
            );
            assert_eq!(
                db.get_kv("pending_sync_token:epoch:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("retained-debt")
            );
            assert_eq!(std::fs::read(&media).unwrap(), b"synthetic preserved media");
        }
        drop(shared);
        drop(db);
        let reopened = core_state(directory.path(), username, &owner).await;
        let mut restarted =
            auth::session::Session::new(directory.path(), username, "https://www.icloud.com", None)
                .await
                .unwrap();
        restarted.bind_principal(&data, None).unwrap();
        restarted
            .session_data
            .insert("session_token".into(), "synthetic-session".into());
        std::fs::write(&cache, serde_json::to_vec(&serde_json::json!({"validated_at":chrono::Utc::now().timestamp(),"account_data":data})).unwrap()).unwrap();
        let restarted = std::sync::Arc::new(tokio::sync::RwLock::new(restarted));
        super::reacquire_session(&restarted, &config, &provider, crate::InputMode::NoInput)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .get_kv("pending_sync_token:epoch:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("retained-debt")
        );
    }

    #[tokio::test]
    async fn initialization_retry_checks_retained_principal_before_service_queries() {
        let directory = tempfile::tempdir().unwrap();
        let data: auth::AccountLoginResponse =
            serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-principal-a"}}))
                .unwrap();
        let mut session = auth::session::Session::new(
            directory.path(),
            "retry@example.invalid",
            "https://www.icloud.com",
            None,
        )
        .await
        .unwrap();
        session.bind_principal(&data, None).unwrap();
        let expected = session.principal().unwrap().to_owned();
        let mut initial = Some(auth::AuthResult {
            session,
            data,
            requires_2fa: false,
        });
        drop(super::take_scoped_pending_auth(&mut initial, Some(&expected)).unwrap());
        for payload in [
            serde_json::json!({"dsInfo":{"dsid":"synthetic-principal-b"}}),
            serde_json::json!({}),
        ] {
            let session = auth::session::Session::new(
                directory.path(),
                "retry@example.invalid",
                "https://www.icloud.com",
                None,
            )
            .await
            .unwrap();
            let mut pending = Some(auth::AuthResult {
                session,
                data: serde_json::from_value(payload).unwrap(),
                requires_2fa: false,
            });
            let error = super::take_scoped_pending_auth(&mut pending, Some(&expected)).unwrap_err();
            assert!(
                error
                    .downcast_ref::<auth::error::AuthError>()
                    .is_some_and(auth::error::AuthError::is_account_identity_changed)
            );
            assert!(pending.is_none());
        }
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
        let session_path = auth::session_file_path(tempdir.path(), username, "com");
        let session_json = br#"{
  "session_token": "tok_abc",
  "trust_token": "trust_xyz",
  "client_id": "client-1",
  "ckdatabasews_url": "https://old.example.test"
}"#;
        tokio::fs::write(&session_path, session_json)
            .await
            .expect("write session metadata");
        let cache_path = auth::validation_cache_file_path(tempdir.path(), username, "com");
        tokio::fs::write(&cache_path, br#"{"validated_at":1}"#)
            .await
            .expect("write validation cache");

        clear_validation_cache_for_reauth(tempdir.path(), username, "com").await;

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
        let source = include_str!("session.rs");
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

    #[tokio::test]
    async fn reauth_cycle_limit_stops_before_authentication_and_keeps_session_lock() {
        let (dir, shared_session) = make_shared_session_for_run_cycle().await;
        let mut config = make_run_cycle_config();
        config.auth.cookie_directory = dir.path().to_path_buf();
        let password_provider: crate::password::PasswordProvider =
            std::sync::Arc::new(|| panic!("reauth limit must stop before password resolution"));
        let notifier = crate::notifications::Notifier::new(None);
        let mut attempts = crate::commands::MAX_REAUTH_ATTEMPTS - 1;

        let error = super::recover_expired_cycle(
            &shared_session,
            &config,
            &password_provider,
            &notifier,
            &mut attempts,
            crate::InputMode::NoInput,
        )
        .await
        .expect_err("exhausted recovery budget must stop the sync loop");

        assert_eq!(attempts, crate::commands::MAX_REAUTH_ATTEMPTS);
        assert_eq!(
            error.to_string(),
            format!(
                "Your iCloud session expired and kei could not refresh it after {} attempts. Run `kei login` and try again.",
                crate::commands::MAX_REAUTH_ATTEMPTS
            )
        );
        let lock_error = auth::session::Session::new(
            dir.path(),
            "test@example.com",
            "https://example.com",
            None,
        )
        .await
        .expect_err("stopping at the limit must retain the existing session lock");
        assert!(
            lock_error
                .downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_lock_contention)
        );
    }

    fn synthetic_cloudkit_auth_account(base_url: &str, dsid: &str) -> auth::AccountLoginResponse {
        serde_json::from_value(serde_json::json!({
            "dsInfo": {"dsid": dsid},
            "webservices": {"ckdatabasews": {"url": base_url}},
            "hsaChallengeRequired": false,
            "hsaTrustedBrowser": true,
            "hasError": false,
            "serviceErrors": [],
            "iCDPEnabled": false,
        }))
        .expect("synthetic account data")
    }

    #[tokio::test]
    async fn unknown_indexing_state_does_not_hold_up_discovery() {
        use wiremock::matchers::path_regex;
        use wiremock::{Mock, ResponseTemplate};

        let server =
            crate::start_wiremock_or_skip!("unknown_indexing_state_does_not_hold_up_discovery");
        let directory = tempfile::tempdir().unwrap();
        let base = server.uri();
        let username = "startup@example.invalid";
        let data = synthetic_cloudkit_auth_account(&base, "fixture");
        tokio::fs::write(
            auth::session_file_path(directory.path(), username, "com"),
            br#"{"session_token":"fixture"}"#,
        )
        .await
        .unwrap();
        let session =
            auth::session::Session::new(directory.path(), username, "https://www.icloud.com", None)
                .await
                .unwrap();
        session.save_validation_cache(&data).await;
        // Every readiness probe gets a state kei does not know. Discovery
        // still finishes in one pass; each cycle decides what waits.
        Mock::given(path_regex("/private/records/query$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"records":[{"fields":{"state":{"value":"UNKNOWN"}}}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(path_regex("/private/zones/list$")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}]}))).expect(1).mount(&server).await;
        Mock::given(path_regex("/shared/zones/list$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"zones":[]})))
            .expect(1)
            .mount(&server)
            .await;
        let mut config = make_run_cycle_config();
        config.auth.username = username.to_owned();
        config.auth.cookie_directory = directory.path().to_owned();
        config.watch.interval = None;
        let password_provider: crate::password::PasswordProvider =
            std::sync::Arc::new(|| panic!("indexing must not invalidate credentials"));
        let notifier = crate::notifications::Notifier::new(None);
        let initialize = crate::auth::endpoints::with_test_endpoint(
            &base,
            Box::pin(super::initialize_libraries(
                auth::AuthResult {
                    session,
                    data: data.clone(),
                    requires_2fa: false,
                },
                crate::retry::RetryConfig::default(),
                &config,
                &password_provider,
                &notifier,
                crate::InputMode::NoInput,
            )),
        );
        let (_, mut service, selected) = initialize.await.unwrap();
        assert_eq!(selected.len(), 1);
        assert!(selected[0].is_qualified_private_primary());
        assert!(!selected[0].last_indexing_finished());
        let libraries = service.all_libraries().await.unwrap();
        let [primary] = libraries.as_slice() else {
            panic!("expected a single published library");
        };
        assert!(!primary.last_indexing_finished());
        server.verify().await;
    }

    #[tokio::test]
    async fn no_auth_method_found_recovers_once_then_terminates_without_acceptance_or_reset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = crate::start_wiremock_or_skip!(
            "no_auth_method_found_recovers_once_then_terminates_without_acceptance_or_reset"
        );
        let directory = tempfile::tempdir().expect("tempdir");
        let username = "cloudkit-auth-recovery@example.invalid";
        let mock_base = server.uri();
        let account_data = synthetic_cloudkit_auth_account(&mock_base, "synthetic-principal-a");

        let session_path = auth::session_file_path(directory.path(), username, "com");
        tokio::fs::write(
            &session_path,
            serde_json::to_vec(&serde_json::json!({
                "session_token": "synthetic-session-token",
                "trust_token": "synthetic-trust-token",
                "client_id": "synthetic-client",
            }))
            .expect("session json"),
        )
        .await
        .expect("persisted session");

        // A fresh cache would let persisted authentication skip accountLogin.
        // Recovery must clear it, then replace it with the validated account.
        let stale_cache = auth::responses::ValidationCache {
            validated_at: chrono::Utc::now().timestamp(),
            account_data: synthetic_cloudkit_auth_account(&mock_base, "stale-cache-principal"),
        };
        let cache_path = auth::validation_cache_file_path(directory.path(), username, "com");
        tokio::fs::write(
            &cache_path,
            serde_json::to_vec(&stale_cache).expect("cache json"),
        )
        .await
        .expect("stale validation cache");

        let owner =
            crate::state::db::account::AccountOwner::authenticated(username, "com", &account_data)
                .expect("account owner");
        let db = core_state(directory.path(), username, &owner).await;
        db.set_kv("sync_token:PrimarySync", "seeded-cursor")
            .await
            .expect("seed cursor");
        db.set_kv("pending_sync_token:epoch:PrimarySync", "seeded-debt")
            .await
            .expect("seed debt");
        drop(db);
        let media = directory.path().join("seeded.jpg");
        tokio::fs::write(&media, b"synthetic seeded media")
            .await
            .expect("seed media");

        let mut session =
            auth::session::Session::new(directory.path(), username, "https://www.icloud.com", None)
                .await
                .expect("initial session");

        let mut config = make_run_cycle_config();
        config.download.directory = directory.path().to_path_buf();
        config.auth.cookie_directory = directory.path().to_path_buf();
        config.auth.username = username.to_string();
        config.auth.domain = crate::types::Domain::Com;
        config.runtime.dry_run = false;
        let password_provider: crate::password::PasswordProvider = std::sync::Arc::new(|| {
            panic!("persisted-session recovery must not request a password")
        });
        let notifier = crate::notifications::Notifier::new(None);

        let login_body = serde_json::to_value(&account_data).expect("login body");
        let unauthorized = serde_json::json!({
            "serverErrorCode": "AUTHENTICATION_FAILED",
            "reason": "no auth method found",
        });
        Mock::given(method("POST"))
            .and(path("/setup/ws/1/validate"))
            .respond_with(ResponseTemplate::new(401).set_body_string("{}"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/setup/ws/1/accountLogin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&login_body))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(
                "/database/1/com.apple.photos.cloud/production/private/records/query",
            ))
            .respond_with(ResponseTemplate::new(401).set_body_json(&unauthorized))
            .mount(&server)
            .await;

        let initial_data = auth::twofa::authenticate_with_token(
            &mut session,
            &auth::endpoints::Endpoints::for_test_base(&mock_base),
        )
        .await
        .expect("initial account login");
        let result = Box::pin(crate::auth::endpoints::with_test_endpoint(
            &mock_base,
            super::initialize_libraries(
                auth::AuthResult {
                    session,
                    data: initial_data,
                    requires_2fa: false,
                },
                crate::retry::RetryConfig {
                    max_retries: 0,
                    base_delay_secs: 0,
                    max_delay_secs: 0,
                },
                &config,
                &password_provider,
                &notifier,
                crate::InputMode::NoInput,
            ),
        ))
        .await;

        let error = result.expect_err("a second CloudKit 401 must terminate initialization");
        assert!(
            error
                .downcast_ref::<crate::icloud::error::ICloudError>()
                .is_some_and(|error| matches!(
                    error,
                    crate::icloud::error::ICloudError::SessionExpired { status: 401 }
                )),
            "expected terminal SessionExpired(401), got: {error:#}"
        );

        let requests = server.received_requests().await.expect("requests");
        let paths: Vec<_> = requests.iter().map(|request| request.url.path()).collect();
        let cloudkit_init = "/database/1/com.apple.photos.cloud/production/private/records/query";
        assert_eq!(
            paths,
            [
                "/setup/ws/1/accountLogin",
                cloudkit_init,
                "/setup/ws/1/validate",
                "/setup/ws/1/accountLogin",
                cloudkit_init,
            ],
            "expected exactly one recovery and no agreement request"
        );
        for request in &requests {
            let body = String::from_utf8_lossy(&request.body).to_ascii_lowercase();
            assert!(
                !body.contains("terms") && !body.contains("agreement") && !body.contains("accept"),
                "recovery must not send an agreement acceptance request"
            );
        }

        let recovered_cache: auth::responses::ValidationCache = serde_json::from_slice(
            &tokio::fs::read(&cache_path)
                .await
                .expect("recovered validation cache"),
        )
        .expect("cache json");
        assert!(
            recovered_cache.validated_at >= stale_cache.validated_at,
            "the recovered validation cache must not predate the stale cache"
        );
        assert_eq!(
            recovered_cache
                .account_data
                .ds_info
                .as_ref()
                .and_then(|info| info.dsid.as_deref()),
            Some("synthetic-principal-a"),
            "the successful persisted login must replace the stale cache"
        );

        let reopened = core_state(directory.path(), username, &owner).await;
        assert_eq!(
            reopened
                .get_kv("sync_token:PrimarySync")
                .await
                .expect("cursor")
                .as_deref(),
            Some("seeded-cursor")
        );
        assert_eq!(
            reopened
                .get_kv("pending_sync_token:epoch:PrimarySync")
                .await
                .expect("debt")
                .as_deref(),
            Some("seeded-debt")
        );
        assert_eq!(
            tokio::fs::read(&media).await.expect("media"),
            b"synthetic seeded media"
        );
    }

    #[tokio::test]
    async fn mid_cycle_reauth_invalidates_cached_success_without_resetting_state() {
        let (dir, shared_session) = make_shared_session_for_run_cycle().await;
        let mut config = make_run_cycle_config();
        config.auth.cookie_directory = dir.path().to_path_buf();
        let cache = auth::validation_cache_file_path(dir.path(), &config.auth.username, "com");
        tokio::fs::write(&cache, b"cached successful validation")
            .await
            .unwrap();
        let generation = shared_session.read().await.generation();
        let state_path = dir.path().join("state-sentinel");
        tokio::fs::write(&state_path, b"durable checkpoint")
            .await
            .unwrap();
        prepare_mid_cycle_reauth(&shared_session, &config)
            .await
            .unwrap();
        assert!(!cache.exists());
        assert_eq!(shared_session.read().await.generation(), generation);
        assert_eq!(
            tokio::fs::read(&state_path).await.unwrap(),
            b"durable checkpoint"
        );
    }
}
