//! iCloud authentication via Apple's SRP-6a variant with optional 2FA.
//!
//! The flow mirrors `icloudpd`'s `PyiCloudService` authentication:
//! session token validation → SRP login → 2FA challenge → session trust.

pub mod endpoints;
pub mod error;
pub mod responses;
pub mod session;
pub mod srp;
pub mod twofa;

use crate::retry::RetryConfig;
use std::future::Future;

/// Retry budget for Apple's auth endpoints (SRP init/complete, 2FA push,
/// 2FA submit). The flow is user-blocking, so we keep this short: three
/// tries total, short backoffs, capped by `Retry-After`.
pub(crate) const AUTH_RETRY_CONFIG: RetryConfig = RetryConfig {
    max_retries: 2,
    base_delay_secs: 2,
    max_delay_secs: 30,
};

use std::path::{Path, PathBuf};

use anyhow::Result;
use secrecy::ExposeSecret;
use uuid::Uuid;

use self::endpoints::Endpoints;
use self::error::AuthError;
pub use self::responses::AccountLoginResponse;
use self::session::Session;
pub use self::session::SharedSession;
pub(crate) use self::session::strip_session_routing_state;

/// Path to the session data file for a given user, without needing a `Session`.
pub fn session_file_path(cookie_dir: &Path, apple_id: &str, realm: &str) -> PathBuf {
    auth_file_path(cookie_dir, apple_id, ".session", realm)
}

/// Path to the validation cache file for a given user.
pub(crate) fn validation_cache_file_path(
    cookie_dir: &Path,
    apple_id: &str,
    realm: &str,
) -> PathBuf {
    auth_file_path(cookie_dir, apple_id, ".cache", realm)
}

/// Path to the persisted cookie jar for a given user.
pub(crate) fn cookiejar_file_path(cookie_dir: &Path, apple_id: &str, realm: &str) -> PathBuf {
    auth_file_path(cookie_dir, apple_id, "", realm)
}

fn auth_file_path(cookie_dir: &Path, apple_id: &str, suffix: &str, realm: &str) -> PathBuf {
    let mut filename = crate::account::namespace(apple_id, realm);
    filename.push_str(suffix);
    cookie_dir.join(filename)
}

/// Result of a successful authentication, including the account data payload.
pub struct AuthResult {
    pub session: Session,
    pub data: AccountLoginResponse,
    /// Whether 2FA was required (and performed) during this authentication.
    pub requires_2fa: bool,
}

impl std::fmt::Debug for AuthResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthResult")
            .field("session", &"<redacted>")
            .field("data", &"<...>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthFlowErrorClass {
    TransientAppleFailure,
    MisdirectedRequest,
    TwoFactorAtGeneration(session::SessionGeneration),
    PushForbidden,
    RetryWithCleanAuth {
        removed_files: usize,
        generation: session::SessionGeneration,
    },
    Other,
}

#[derive(Debug, thiserror::Error)]
#[error("Apple rejected persisted authentication state")]
struct RetryWithCleanAuth {
    removed_files: usize,
    generation: session::SessionGeneration,
}

#[derive(Debug)]
struct TwoFactorGeneration {
    generation: session::SessionGeneration,
}

impl std::fmt::Display for TwoFactorGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        AuthError::TwoFactorRequired.fmt(f)
    }
}

pub(crate) fn two_factor_generation(error: &anyhow::Error) -> Option<session::SessionGeneration> {
    match classify_auth_flow_error(error) {
        AuthFlowErrorClass::TwoFactorAtGeneration(generation) => Some(generation),
        _ => None,
    }
}

fn is_forbidden(error: &anyhow::Error) -> bool {
    classify_auth_flow_error(error) == AuthFlowErrorClass::PushForbidden
}

async fn map_push_error(session: &Session, error: anyhow::Error) -> Result<anyhow::Error> {
    if session.loaded_persisted_auth() && is_forbidden(&error) {
        let removed_files = session
            .discard_persisted_auth()
            .await
            .map_err(|error| error.context("Could not discard stale iCloud authentication state"))?
            .len();
        return Ok(RetryWithCleanAuth {
            removed_files,
            generation: session.generation(),
        }
        .into());
    }
    Ok(error)
}

async fn retry_once_after_stale_auth<T, F, Fut>(
    initial_generation: Option<session::SessionGeneration>,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut(Option<session::SessionGeneration>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    match attempt(initial_generation).await {
        Err(error) => match classify_auth_flow_error(&error) {
            AuthFlowErrorClass::RetryWithCleanAuth {
                removed_files,
                generation,
            } => {
                tracing::warn!(
                    removed_files,
                    "Apple rejected persisted authentication state; retrying once from clean state"
                );
                attempt(Some(generation)).await
            }
            _ => Err(error),
        },
        result => result,
    }
}

/// Classify errors that change auth orchestration behavior.
///
/// `anyhow::Error` remains the return type so the original error chain is
/// preserved. This helper only owns local branch decisions for transient auth
/// guidance and bounded 421 HTTP-pool recovery.
fn classify_auth_flow_error(err: &anyhow::Error) -> AuthFlowErrorClass {
    if let Some(error) = err.downcast_ref::<TwoFactorGeneration>() {
        return AuthFlowErrorClass::TwoFactorAtGeneration(error.generation);
    }
    if let Some(error) = err.downcast_ref::<RetryWithCleanAuth>() {
        return AuthFlowErrorClass::RetryWithCleanAuth {
            removed_files: error.removed_files,
            generation: error.generation,
        };
    }
    let Some(auth_err) = err.downcast_ref::<AuthError>() else {
        return AuthFlowErrorClass::Other;
    };
    if auth_err.is_transient_apple_failure() {
        AuthFlowErrorClass::TransientAppleFailure
    } else if auth_err.is_misdirected_request() {
        AuthFlowErrorClass::MisdirectedRequest
    } else if matches!(auth_err, AuthError::ApiError { code: 403, .. }) {
        AuthFlowErrorClass::PushForbidden
    } else {
        AuthFlowErrorClass::Other
    }
}

/// Top-level authentication orchestrator.
///
/// 1. Tries to validate the existing session token.
/// 2. If invalid, obtains a password and performs SRP authentication.
/// 3. Authenticates with the resulting token.
/// 4. Checks if 2FA is required; if `code` is `Some`, submits it directly,
///    otherwise prompts the user interactively.
/// 5. Returns the authenticated session and account data.
///
/// When `code` is `None` and the captured input mode disallows prompts,
/// returns `AuthError::TwoFactorRequired` so the caller can handle it.
#[allow(
    dead_code,
    reason = "kept as the default-input wrapper for internal auth integrations; CLI owners use the startup snapshot"
)]
pub async fn authenticate(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &crate::password::PasswordProvider,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
    code: Option<&str>,
) -> Result<AuthResult> {
    authenticate_with_modes(
        cookie_dir,
        apple_id,
        password_provider,
        domain,
        client_id,
        timeout_secs,
        code,
        crate::personality::Mode::Off,
        crate::InputMode::detect(),
        None,
    )
    .await
}

/// Authenticate using the process input mode captured at startup.
pub(crate) async fn authenticate_in_input_mode(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &crate::password::PasswordProvider,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
    code: Option<&str>,
    input_mode: crate::InputMode,
) -> Result<AuthResult> {
    authenticate_with_modes(
        cookie_dir,
        apple_id,
        password_provider,
        domain,
        client_id,
        timeout_secs,
        code,
        crate::personality::Mode::Off,
        input_mode,
        None,
    )
    .await
}

/// Explicit legacy adoption requires a fresh password/2FA exchange, even if a
/// valid session exists in the new namespace. Legacy artifacts are never read.
pub(crate) async fn authenticate_fresh_for_adoption(
    directory: &Path,
    username: &str,
    provider: &crate::password::PasswordProvider,
    domain: &str,
    code: Option<&str>,
    input_mode: crate::InputMode,
) -> Result<AuthResult> {
    let endpoints = Endpoints::for_domain(domain)?;
    let session = Session::new_fresh(directory, username, endpoints.home).await?;
    authenticate_inner(
        session,
        &endpoints,
        username,
        provider,
        domain,
        None,
        code,
        crate::personality::Mode::Off,
        input_mode,
    )
    .await
}

/// Like `authenticate`, but threads the friendly-mode flag through so the
/// 2FA prompt can print a contextual line above the bare prompt. Off-mode
/// behaviour is identical to `authenticate`.
#[allow(
    clippy::too_many_arguments,
    reason = "mode is a UX gate that doesn't fit any existing struct param without muddying its semantics"
)]
#[allow(
    dead_code,
    reason = "kept as the output-mode wrapper for internal auth integrations; CLI owners also pass the startup input snapshot"
)]
pub async fn authenticate_with_mode(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &crate::password::PasswordProvider,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
    code: Option<&str>,
    mode: crate::personality::Mode,
) -> Result<AuthResult> {
    authenticate_with_modes(
        cookie_dir,
        apple_id,
        password_provider,
        domain,
        client_id,
        timeout_secs,
        code,
        mode,
        crate::InputMode::detect(),
        None,
    )
    .await
}

/// Authenticate with caller-resolved output and input modes.
#[allow(
    clippy::too_many_arguments,
    reason = "input and personality modes are independent startup policies"
)]
pub(crate) async fn authenticate_with_modes(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &crate::password::PasswordProvider,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
    code: Option<&str>,
    mode: crate::personality::Mode,
    input_mode: crate::InputMode,
    expected_generation: Option<session::SessionGeneration>,
) -> Result<AuthResult> {
    #[cfg(debug_assertions)]
    if std::env::var("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS").as_deref() == Ok("1") {
        tracing::warn!("Offline 2FA-required test seam enabled; skipping Apple authentication");
        return Err(AuthError::TwoFactorRequired.into());
    }

    let endpoints = Endpoints::for_domain(domain)?;
    retry_once_after_stale_auth(expected_generation, |generation| {
        let client_id = client_id.clone();
        let endpoints = &endpoints;
        async move {
            let session = match generation {
                Some(generation) => {
                    Session::new_after_release(
                        cookie_dir,
                        apple_id,
                        endpoints.home,
                        timeout_secs,
                        generation,
                    )
                    .await?
                }
                None => Session::new(cookie_dir, apple_id, endpoints.home, timeout_secs).await?,
            };
            authenticate_inner(
                session,
                endpoints,
                apple_id,
                password_provider,
                domain,
                client_id,
                code,
                mode,
                input_mode,
            )
            .await
        }
    })
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "mode is a UX gate threaded through to the 2FA prompt narration"
)]
async fn authenticate_inner(
    mut session: Session,
    endpoints: &Endpoints,
    apple_id: &str,
    password_provider: &crate::password::PasswordProvider,
    domain: &str,
    client_id: Option<String>,
    code: Option<&str>,
    mode: crate::personality::Mode,
    input_mode: crate::InputMode,
) -> Result<AuthResult> {
    // Prefer persisted client_id to maintain session continuity across runs
    let client_id = session
        .client_id()
        .map(str::to_owned)
        .or(client_id)
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.set_client_id(&client_id);

    let mut data: Option<AccountLoginResponse> = None;
    let mut validated_existing_session = false;
    let has_session_token = session.session_data.contains_key("session_token");

    // Fast path: if we validated recently, skip the Apple /validate call entirely.
    // The cookies and session token are still in the session file; if they've
    // actually gone stale, the first CloudKit call will 421 and trigger re-auth.
    if has_session_token
        && code.is_none()
        && let Some(cached) = session
            .load_validation_cache(responses::VALIDATION_CACHE_GRACE_SECS)
            .await
    {
        tracing::debug!("Session validated recently, skipping /validate call");
        return Ok(AuthResult {
            session,
            data: cached,
            requires_2fa: false,
        });
    }

    // The 421-recovery flow below is bounded. Each branch takes at most one
    // action and then advances:
    //   1. /validate 421  → reset HTTP pool, fall through to /accountLogin
    //   2. /accountLogin 421 after pool_reset → fall through to SRP (no
    //      second reset because pool_reset is sticky)
    //   3. /accountLogin 421 without prior pool_reset → reset pool, fall
    //      through to SRP
    //   4. SRP → one final /accountLogin; if that 421s we reset the pool
    //      and retry /accountLogin exactly once more
    // Max pool resets across the function: 2. Max /accountLogin calls: 3.
    // No branch loops back to an earlier stage, so the function cannot
    // diverge.
    let mut pool_reset = false;
    if has_session_token {
        tracing::debug!("Checking session token validity");
        match twofa::validate_token(&mut session, endpoints).await {
            Ok(d) => {
                tracing::debug!("Existing session token is valid");
                validated_existing_session = true;
                data = Some(d);
            }
            Err(e) => match classify_auth_flow_error(&e) {
                AuthFlowErrorClass::TransientAppleFailure => {
                    return Err(e.context(
                        "Apple's authentication service is temporarily failing (HTTP 429/5xx). Wait a few minutes and retry.",
                    ));
                }
                AuthFlowErrorClass::MisdirectedRequest => {
                    tracing::warn!(
                        error = %e,
                        "validate returned 421 Misdirected Request; resetting HTTP pool \
                         before accountLogin/SRP"
                    );
                    session.reset_http_clients()?;
                    pool_reset = true;
                }
                _ => {
                    tracing::debug!(
                        error = %e,
                        "Invalid authentication token, will log in from scratch"
                    );
                }
            },
        }
    }

    // Try /accountLogin as a fallback before SRP. The /validate endpoint
    // above is strict and often rejects sessions that /accountLogin accepts
    // (e.g. post-2FA trusted sessions loaded from disk). /accountLogin
    // sends dsWebAuthToken + trustToken and is more lenient -- it succeeds
    // for most persisted sessions, avoiding unnecessary SRP handshakes.
    // This is critical because Apple rate-limits SRP to ~10 auths per
    // rolling window.
    if data.is_none() && has_session_token {
        tracing::debug!("Session token exists, trying accountLogin before SRP");
        match twofa::authenticate_with_token(&mut session, endpoints).await {
            Ok(d) => {
                tracing::debug!("accountLogin succeeded, skipping SRP");
                data = Some(d);
            }
            Err(e) => match classify_auth_flow_error(&e) {
                AuthFlowErrorClass::TransientAppleFailure => {
                    return Err(e.context(
                        "Apple's auth service is returning transient errors (HTTP 429/5xx). \
                         Wait a few minutes and retry",
                    ));
                }
                AuthFlowErrorClass::MisdirectedRequest => {
                    if pool_reset {
                        tracing::warn!(
                            error = %e,
                            "accountLogin also returned 421 Misdirected Request after pool reset"
                        );
                    } else {
                        tracing::warn!(
                            error = %e,
                            "accountLogin returned 421 Misdirected Request; resetting HTTP pool \
                             before SRP"
                        );
                        session.reset_http_clients()?;
                    }
                }
                _ => {
                    tracing::debug!(
                        error = %e,
                        "accountLogin failed, falling back to SRP"
                    );
                }
            },
        }
    }

    // If validate and accountLogin both failed (including persistent 421),
    // fall through to SRP. SRP is the canonical path for re-minting session
    // cookies, and trust_token is preserved across the session (via
    // `strip_session_routing_state`) so 2FA is skipped in the common case.
    if data.is_none() {
        let password = crate::password::invoke_password_provider(password_provider)
            .await
            .ok_or_else(|| {
                AuthError::FailedLogin(
                    "No password was available. Check the password-source error above.".into(),
                )
            })?;

        tracing::debug!("Authenticating");

        srp::authenticate_srp(
            &mut session,
            endpoints,
            apple_id,
            password.expose_secret(),
            &client_id,
            domain,
        )
        .await?;
        // `password` (SecretString) dropped here, zeroing memory

        // Post-SRP cookies are fresh, so a 421 here is narrow (HTTP/2 pool
        // still pinned to the wrong partition). Reset the pool once and retry
        // so the caller doesn't see an AuthError::ServiceError that the
        // sync_loop init-retry (which matches on ICloudError) would miss.
        let account_data = match twofa::authenticate_with_token(&mut session, endpoints).await {
            Ok(d) => d,
            Err(e) if classify_auth_flow_error(&e) == AuthFlowErrorClass::MisdirectedRequest => {
                tracing::warn!(
                    error = %e,
                    "accountLogin returned 421 Misdirected Request after SRP; \
                     resetting HTTP pool and retrying once"
                );
                session.reset_http_clients()?;
                twofa::authenticate_with_token(&mut session, endpoints).await?
            }
            Err(e) => return Err(e),
        };
        data = Some(account_data);
    }

    let data =
        data.ok_or_else(|| anyhow::anyhow!("Apple authentication did not return account data."))?;

    let requires_2fa = !validated_existing_session && check_requires_2fa(&data);
    if requires_2fa {
        tracing::info!("Two-factor authentication is required");

        // Headless with no code: bail without any Apple API calls.
        // The user triggers the push manually via `get-code`.
        if code.is_none() && !input_mode.can_prompt() {
            return Err(anyhow::Error::new(AuthError::TwoFactorRequired).context(
                TwoFactorGeneration {
                    generation: session.generation(),
                },
            ));
        }

        let verified = if let Some(c) = code {
            // Headless: code provided directly (e.g. submit-code subcommand).
            // Do NOT trigger a push — it would invalidate the code being submitted.
            twofa::submit_2fa_code(&mut session, endpoints, &client_id, domain, c).await?
        } else {
            // Interactive: prompt on stdin (terminal confirmed above).
            // Always trigger an explicit push before prompting. SRP pushes
            // a code for some accounts but not all — the explicit push
            // ensures every account gets one. Apple deduplicates, so
            // accounts that already got a code from SRP won't see a second.
            if let Err(error) =
                twofa::trigger_push_notification(&mut session, endpoints, &client_id, domain).await
            {
                let error = map_push_error(&session, error).await?;
                if matches!(
                    classify_auth_flow_error(&error),
                    AuthFlowErrorClass::RetryWithCleanAuth { .. }
                        | AuthFlowErrorClass::PushForbidden
                ) {
                    return Err(error);
                }
                tracing::warn!(error = %error, "Failed to trigger push notification");
            }

            const MAX_WRONG_CODES: u32 = 3;
            let mut wrong_codes = 0u32;
            let mut verified = false;
            loop {
                let input = twofa::prompt_2fa_code(mode).await?;
                if input.is_empty() {
                    // User didn't receive a code - trigger explicit push.
                    if let Err(e) = twofa::trigger_push_notification(
                        &mut session,
                        endpoints,
                        &client_id,
                        domain,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "Failed to trigger push notification");
                    }
                    tracing::info!("Code requested - check your trusted devices");
                    continue;
                }
                if twofa::submit_2fa_code(&mut session, endpoints, &client_id, domain, &input)
                    .await?
                {
                    verified = true;
                    break;
                }
                wrong_codes += 1;
                if wrong_codes >= MAX_WRONG_CODES {
                    break;
                }
                tracing::warn!(
                    attempt = wrong_codes,
                    max = MAX_WRONG_CODES,
                    "Wrong code, please try again"
                );
            }
            verified
        };

        if !verified {
            return Err(AuthError::TwoFactorFailed("2FA verification failed".into()).into());
        }

        twofa::trust_session(&mut session, endpoints, &client_id, domain).await?;
        // Re-authenticate to get fresh account data with 2FA-elevated privileges
        let account_data = twofa::authenticate_with_token(&mut session, endpoints).await?;

        tracing::info!("Authentication completed successfully");
        session.save_validation_cache(&account_data).await;
        return Ok(AuthResult {
            session,
            data: account_data,
            requires_2fa: true,
        });
    }

    tracing::info!("Authentication completed successfully");
    session.save_validation_cache(&data).await;
    Ok(AuthResult {
        session,
        data,
        requires_2fa: false,
    })
}

/// Trigger a 2FA push notification to trusted devices.
///
/// Performs SRP authentication (if needed) to establish a valid session,
/// then sends the push notification via Apple's bridge endpoint. This is
/// the `get-code` command's backend.
pub async fn send_2fa_push(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &crate::password::PasswordProvider,
    domain: &str,
) -> Result<()> {
    let endpoints = Endpoints::for_domain(domain)?;
    retry_once_after_stale_auth(None, |generation| {
        send_2fa_push_inner(
            cookie_dir,
            apple_id,
            password_provider,
            domain,
            &endpoints,
            generation,
        )
    })
    .await
}

async fn send_2fa_push_inner(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &crate::password::PasswordProvider,
    domain: &str,
    endpoints: &Endpoints,
    expected_generation: Option<session::SessionGeneration>,
) -> Result<()> {
    let mut session = match expected_generation {
        Some(generation) => {
            Session::new_after_release(cookie_dir, apple_id, endpoints.home, None, generation)
                .await?
        }
        None => Session::new(cookie_dir, apple_id, endpoints.home, None).await?,
    };

    let client_id = session
        .client_id()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.set_client_id(&client_id);

    let mut data: Option<AccountLoginResponse> = None;
    let has_session_token = session.session_data.contains_key("session_token");

    if has_session_token
        && session
            .load_validation_cache(responses::VALIDATION_CACHE_GRACE_SECS)
            .await
            .is_some()
    {
        tracing::debug!("Session validated recently; no 2FA push is needed for cached session");
        return already_authenticated();
    }

    let mut pool_reset = false;
    if data.is_none() && has_session_token {
        match twofa::validate_token(&mut session, endpoints).await {
            Ok(d) => {
                session.save_validation_cache(&d).await;
                tracing::debug!("Existing session token is valid; no 2FA push is needed");
                return already_authenticated();
            }
            Err(e) => match classify_auth_flow_error(&e) {
                AuthFlowErrorClass::TransientAppleFailure => {
                    return Err(e.context(
                        "Apple's authentication service is temporarily failing (HTTP 429/5xx). Wait a few minutes and retry.",
                    ));
                }
                AuthFlowErrorClass::MisdirectedRequest => {
                    tracing::warn!(
                        error = %e,
                        "validate returned 421 Misdirected Request; resetting HTTP pool \
                         before accountLogin/SRP"
                    );
                    session.reset_http_clients()?;
                    pool_reset = true;
                }
                _ => {}
            },
        }
    }

    // Try accountLogin before SRP (same rationale as authenticate_inner:
    // validate_token is strict, accountLogin is lenient).
    if data.is_none() && has_session_token {
        match twofa::authenticate_with_token(&mut session, endpoints).await {
            Ok(d) => {
                data = Some(d);
            }
            Err(e) => match classify_auth_flow_error(&e) {
                AuthFlowErrorClass::MisdirectedRequest if !pool_reset => {
                    tracing::warn!(
                        error = %e,
                        "accountLogin returned 421 Misdirected Request; resetting HTTP pool \
                         before SRP"
                    );
                    session.reset_http_clients()?;
                }
                _ => {
                    tracing::debug!(
                        error = %e,
                        "accountLogin failed during send_2fa_push, falling back to SRP"
                    );
                }
            },
        }
    }

    if data.is_none() {
        let password = crate::password::invoke_password_provider(password_provider)
            .await
            .ok_or_else(|| {
                AuthError::FailedLogin(
                    "No password was available. Check the password-source error above.".into(),
                )
            })?;
        srp::authenticate_srp(
            &mut session,
            endpoints,
            apple_id,
            password.expose_secret(),
            &client_id,
            domain,
        )
        .await?;
        let account_data = match twofa::authenticate_with_token(&mut session, endpoints).await {
            Ok(d) => d,
            Err(e) if classify_auth_flow_error(&e) == AuthFlowErrorClass::MisdirectedRequest => {
                tracing::warn!(
                    error = %e,
                    "accountLogin returned 421 Misdirected Request after SRP; \
                     resetting HTTP pool and retrying once"
                );
                session.reset_http_clients()?;
                twofa::authenticate_with_token(&mut session, endpoints).await?
            }
            Err(e) => return Err(e),
        };
        data = Some(account_data);
    }

    let data =
        data.ok_or_else(|| anyhow::anyhow!("Apple authentication did not return account data."))?;

    if !check_requires_2fa(&data) {
        return already_authenticated();
    }

    match twofa::trigger_push_notification(&mut session, endpoints, &client_id, domain).await {
        Ok(()) => Ok(()),
        Err(error) => Err(map_push_error(&session, error).await?),
    }
}

fn already_authenticated() -> Result<()> {
    anyhow::bail!("This iCloud session is already authenticated; no 2FA code is needed.")
}

/// Check if the current session token is still valid by calling Apple's
/// validate endpoint. Returns `true` if valid, `false` if expired.
pub async fn validate_session(session: &mut Session, domain: &str) -> Result<bool> {
    let endpoints = Endpoints::for_domain(domain)?;
    if let Some(cached) = cached_validation(session).await {
        session.bind_principal(&cached, None)?;
        tracing::debug!("Session validated recently, skipping idle re-validation");
        return Ok(true);
    }

    match twofa::validate_token(session, &endpoints).await {
        Ok(d) => {
            session.bind_principal(&d, None)?;
            session.save_validation_cache(&d).await;
            Ok(true)
        }
        Err(_) => {
            // /validate is strict; try /accountLogin as a lenient fallback.
            // A session is valid if accountLogin succeeds and 2FA is not required
            // (i.e. the trust token is still accepted).
            match twofa::authenticate_with_token(session, &endpoints).await {
                Ok(d) => {
                    session.bind_principal(&d, None)?;
                    if check_requires_2fa(&d) {
                        return Ok(false);
                    }
                    // If Apple rerouted the account to a different CloudKit
                    // partition, the stored ckdatabasews URL is stale. Return
                    // false to force full re-auth, which rebuilds PhotosService
                    // with the new URL.
                    let fresh_url = d
                        .webservices
                        .as_ref()
                        .and_then(|ws| ws.ckdatabasews.as_ref())
                        .map(|ep| ep.url.as_str());
                    let stored_url = session.session_data.get("ckdatabasews_url");
                    if let (Some(fresh), Some(stored)) = (fresh_url, stored_url)
                        && fresh != stored
                    {
                        tracing::info!(
                            old_url = %stored,
                            new_url = %fresh,
                            "CloudKit partition changed, forcing full re-auth"
                        );
                        return Ok(false);
                    }
                    session.save_validation_cache(&d).await;
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        }
    }
}

async fn cached_validation(session: &Session) -> Option<AccountLoginResponse> {
    if !session.session_data.contains_key("session_token") {
        return None;
    }
    session
        .load_validation_cache(responses::VALIDATION_CACHE_GRACE_SECS)
        .await
}

/// Apple's HSA2 (two-step verification v2) requires all three conditions:
/// the account uses `HSAv2`, the browser isn't trusted yet, and the account
/// has a device capable of receiving verification codes.
fn check_requires_2fa(data: &AccountLoginResponse) -> bool {
    let (hsa_version, has_qualifying_device) = match &data.ds_info {
        Some(ds) => (ds.hsa_version, ds.has_i_cloud_qualifying_device),
        None => (0, false),
    };

    hsa_version == 2
        && (data.hsa_challenge_required || !data.hsa_trusted_browser)
        && has_qualifying_device
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::responses::{AccountLoginResponse, DsInfo};
    use secrecy::SecretString;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    fn make_response(
        hsa_version: i64,
        challenge: bool,
        trusted: bool,
        qualifying: bool,
    ) -> AccountLoginResponse {
        AccountLoginResponse {
            ds_info: Some(DsInfo {
                hsa_version,
                dsid: None,
                has_i_cloud_qualifying_device: qualifying,
            }),
            webservices: None,
            hsa_challenge_required: challenge,
            hsa_trusted_browser: trusted,
            domain_to_use: None,
            has_error: false,
            service_errors: Vec::new(),
            i_cdp_enabled: false,
        }
    }

    fn classify_auth_flow_error_for(err: AuthError) -> AuthFlowErrorClass {
        let err = anyhow::Error::new(err);
        classify_auth_flow_error(&err)
    }

    #[test]
    fn classify_auth_flow_error_detects_421_misdirected_request() {
        assert_eq!(
            classify_auth_flow_error_for(AuthError::ApiError {
                code: 421,
                message: "misdirected".into(),
            }),
            AuthFlowErrorClass::MisdirectedRequest
        );
    }

    #[test]
    fn classify_auth_flow_error_detects_429_and_503_transients() {
        assert_eq!(
            classify_auth_flow_error_for(AuthError::ApiError {
                code: 429,
                message: "rate limited".into(),
            }),
            AuthFlowErrorClass::TransientAppleFailure
        );
        assert_eq!(
            classify_auth_flow_error_for(AuthError::ApiError {
                code: 503,
                message: "unavailable".into(),
            }),
            AuthFlowErrorClass::TransientAppleFailure
        );
    }

    #[test]
    fn classify_auth_flow_error_treats_non_auth_errors_as_other() {
        let err = anyhow::anyhow!("plain failure");
        assert_eq!(classify_auth_flow_error(&err), AuthFlowErrorClass::Other);
    }

    #[test]
    fn classify_auth_flow_error_detects_context_wrapped_auth_errors() {
        let misdirected = anyhow::Error::new(AuthError::ApiError {
            code: 421,
            message: "misdirected".into(),
        })
        .context("validate token");
        assert_eq!(
            classify_auth_flow_error(&misdirected),
            AuthFlowErrorClass::MisdirectedRequest
        );

        let transient = anyhow::Error::new(AuthError::ServiceError {
            code: "http_503".into(),
            message: "unavailable".into(),
        })
        .context("accountLogin");
        assert_eq!(
            classify_auth_flow_error(&transient),
            AuthFlowErrorClass::TransientAppleFailure
        );
    }

    #[tokio::test]
    async fn two_factor_required_carries_session_generation() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let session = Session::new(
            cookies.path(),
            "generation@example.com",
            "https://example.com",
            None,
        )
        .await
        .expect("session");
        let generation = session.generation();
        let error = anyhow::Error::new(AuthError::TwoFactorRequired)
            .context(TwoFactorGeneration { generation });

        assert_eq!(two_factor_generation(&error), Some(generation));
        assert!(
            error
                .downcast_ref::<AuthError>()
                .is_some_and(AuthError::is_two_factor_required)
        );
        assert_eq!(
            classify_auth_flow_error(&error),
            AuthFlowErrorClass::TwoFactorAtGeneration(generation)
        );
        assert!(error.to_string().contains("Two-factor authentication"));
    }

    #[tokio::test]
    async fn map_push_error_only_cleans_persisted_403() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let apple_id = "persisted@example.com";
        let files = session::persisted_auth_files(cookies.path(), apple_id, "com");
        tokio::fs::write(&files[1], b"stale")
            .await
            .expect("session file");
        let persisted = Session::new(cookies.path(), apple_id, "https://example.com", None)
            .await
            .expect("persisted session");

        let retry = map_push_error(
            &persisted,
            AuthError::ApiError {
                code: 403,
                message: "forbidden".into(),
            }
            .into(),
        )
        .await
        .expect("map persisted 403");

        assert!(retry.downcast_ref::<RetryWithCleanAuth>().is_some());
        assert!(files.iter().all(|path| !path.exists()));
        drop(persisted);

        let clean = Session::new(cookies.path(), apple_id, "https://example.com", None)
            .await
            .expect("clean session");
        let forbidden = map_push_error(
            &clean,
            AuthError::ApiError {
                code: 403,
                message: "forbidden".into(),
            }
            .into(),
        )
        .await
        .expect("map clean 403");
        assert!(is_forbidden(&forbidden));

        let unavailable = map_push_error(
            &clean,
            AuthError::ApiError {
                code: 503,
                message: "unavailable".into(),
            }
            .into(),
        )
        .await
        .expect("map non-403");
        assert!(unavailable.downcast_ref::<RetryWithCleanAuth>().is_none());
    }

    #[tokio::test]
    async fn stale_auth_retry_runs_once_more() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let session = Session::new(
            cookies.path(),
            "generation@example.com",
            "https://example.com",
            None,
        )
        .await
        .expect("session");
        let generation = session.generation();
        drop(session);
        let attempts = Arc::new(AtomicUsize::new(0));

        let result = retry_once_after_stale_auth(None, |expected_generation| {
            let attempts = Arc::clone(&attempts);
            async move {
                match attempts.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert_eq!(expected_generation, None);
                        Err(RetryWithCleanAuth {
                            removed_files: 3,
                            generation,
                        }
                        .into())
                    }
                    1 => {
                        assert_eq!(expected_generation, Some(generation));
                        Ok(42)
                    }
                    attempt => panic!("unexpected attempt {attempt}"),
                }
            }
        })
        .await
        .expect("clean retry");

        assert_eq!(result, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ordinary_auth_failure_does_not_retry() {
        let attempts = Arc::new(AtomicUsize::new(0));

        let err = retry_once_after_stale_auth(None, |expected_generation| {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                assert_eq!(expected_generation, None);
                Err::<(), _>(
                    AuthError::ApiError {
                        code: 403,
                        message: "forbidden".into(),
                    }
                    .into(),
                )
            }
        })
        .await
        .expect_err("clean failure");

        assert!(is_forbidden(&err));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stale_auth_retry_surfaces_second_failure() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let session = Session::new(
            cookies.path(),
            "generation@example.com",
            "https://example.com",
            None,
        )
        .await
        .expect("session");
        let generation = session.generation();
        drop(session);
        let attempts = Arc::new(AtomicUsize::new(0));

        let err = retry_once_after_stale_auth(None, |expected_generation| {
            let attempts = Arc::clone(&attempts);
            async move {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    assert_eq!(expected_generation, None);
                    Err::<(), anyhow::Error>(
                        RetryWithCleanAuth {
                            removed_files: 1,
                            generation,
                        }
                        .into(),
                    )
                } else {
                    assert_eq!(expected_generation, Some(generation));
                    Err(AuthError::ApiError {
                        code: 403,
                        message: "still forbidden".into(),
                    }
                    .into())
                }
            }
        })
        .await
        .expect_err("second failure");

        assert!(is_forbidden(&err));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_requires_2fa_all_conditions_met() {
        let resp = make_response(2, true, false, true);
        assert!(check_requires_2fa(&resp));
    }

    #[test]
    fn test_requires_2fa_trusted_no_challenge() {
        let resp = make_response(2, false, true, true);
        assert!(!check_requires_2fa(&resp));
    }

    #[test]
    fn test_requires_2fa_wrong_hsa_version() {
        let resp = make_response(1, true, false, true);
        assert!(!check_requires_2fa(&resp));
    }

    #[test]
    fn test_requires_2fa_no_qualifying_device() {
        let resp = make_response(2, true, false, false);
        assert!(!check_requires_2fa(&resp));
    }

    #[test]
    fn test_requires_2fa_no_ds_info() {
        let resp = AccountLoginResponse {
            ds_info: None,
            webservices: None,
            hsa_challenge_required: true,
            hsa_trusted_browser: false,
            domain_to_use: None,
            has_error: false,
            service_errors: Vec::new(),
            i_cdp_enabled: false,
        };
        assert!(!check_requires_2fa(&resp));
    }

    #[test]
    fn test_requires_2fa_untrusted_no_challenge() {
        // Not trusted + no explicit challenge = still requires 2FA
        let resp = make_response(2, false, false, true);
        assert!(check_requires_2fa(&resp));
    }

    #[test]
    fn test_requires_2fa_challenged_and_trusted() {
        // Both challenged and trusted — still requires 2FA because the
        // challenge flag alone is sufficient
        let resp = make_response(2, true, true, true);
        assert!(check_requires_2fa(&resp));
    }

    #[test]
    fn test_session_file_path_sanitizes_username() {
        let dir = Path::new("/tmp/cookies");
        let path = session_file_path(dir, "user@icloud.com", "com");
        assert_eq!(
            path,
            dir.join(format!(
                "{}.session",
                crate::account::namespace("user@icloud.com", "com")
            ))
        );
    }

    #[test]
    fn test_session_file_path_handles_unicode_and_symbols() {
        let dir = Path::new("/data");
        // Exact punctuation is retained in the private namespace fingerprint.
        let path = session_file_path(dir, "user+tag@example.co.uk", "com");
        assert_eq!(
            path,
            dir.join(format!(
                "{}.session",
                crate::account::namespace("user+tag@example.co.uk", "com")
            ))
        );
    }

    #[test]
    fn test_session_file_path_empty_username_leaves_bare_extension() {
        // The helper is total, though command owners reject empty usernames.
        let dir = Path::new("/var/cookies");
        let path = session_file_path(dir, "", "com");
        assert_eq!(
            path,
            dir.join(format!("{}.session", crate::account::namespace("", "com")))
        );
    }

    #[tokio::test]
    async fn adoption_auth_calls_password_provider_instead_of_reusing_a_valid_cache() {
        let directory = tempfile::tempdir().unwrap();
        let username = "adoption@example.invalid";
        let session = session_file_path(directory.path(), username, "com");
        let cache = validation_cache_file_path(directory.path(), username, "com");
        std::fs::write(
            &session,
            br#"{"session_token":"synthetic-valid-cache-token"}"#,
        )
        .unwrap();
        std::fs::write(&cache, serde_json::to_vec(&serde_json::json!({"validated_at":chrono::Utc::now().timestamp(),"account_data":{"dsInfo":{"dsid":"synthetic-cached-provider"}}})).unwrap()).unwrap();
        let legacy = directory.path().join("adoptionexampleinvalid.session");
        std::fs::write(&legacy, b"synthetic legacy auth preserved").unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&called);
        let provider: crate::password::PasswordProvider = Arc::new(move || {
            observed.store(true, Ordering::SeqCst);
            None
        });
        let error = authenticate_fresh_for_adoption(
            directory.path(),
            username,
            &provider,
            "com",
            None,
            crate::InputMode::NoInput,
        )
        .await
        .unwrap_err();
        assert!(called.load(Ordering::SeqCst));
        assert!(error.to_string().contains("No password was available"));
        assert!(!session.exists());
        assert!(!cache.exists());
        assert_eq!(
            std::fs::read(legacy).unwrap(),
            b"synthetic legacy auth preserved"
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_unsupported_domain_before_password_lookup() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let called = Arc::new(AtomicBool::new(false));
        let called_for_provider = Arc::clone(&called);
        let provider: crate::password::PasswordProvider = Arc::new(move || {
            called_for_provider.store(true, Ordering::SeqCst);
            Some(SecretString::from("should-not-be-read"))
        });

        let err = authenticate(
            cookies.path(),
            "user@example.com",
            &provider,
            "unsupported",
            None,
            None,
            None,
        )
        .await
        .expect_err("unsupported domain should fail before session auth");

        assert!(
            err.to_string().contains("Unsupported iCloud domain"),
            "unexpected error: {err:#}"
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "password provider must not run when endpoint resolution fails"
        );
    }

    #[tokio::test]
    async fn send_2fa_push_rejects_unsupported_domain_before_password_lookup() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let called = Arc::new(AtomicBool::new(false));
        let called_for_provider = Arc::clone(&called);
        let provider: crate::password::PasswordProvider = Arc::new(move || {
            called_for_provider.store(true, Ordering::SeqCst);
            Some(SecretString::from("should-not-be-read"))
        });

        let err = send_2fa_push(cookies.path(), "user@example.com", &provider, "unsupported")
            .await
            .expect_err("unsupported domain should fail before SRP");

        assert!(
            err.to_string().contains("Unsupported iCloud domain"),
            "unexpected error: {err:#}"
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "password provider must not run when endpoint resolution fails"
        );
    }

    #[tokio::test]
    async fn send_2fa_push_treats_fresh_validation_cache_as_authenticated() {
        let server = crate::start_wiremock_or_skip!();
        let endpoints = Endpoints::for_test_base(&server.uri());
        let cookies = tempfile::tempdir().expect("tempdir");
        let apple_id = "cached-get-code@example.com";
        tokio::fs::write(
            session_file_path(cookies.path(), apple_id, "com"),
            r#"{"session_token":"valid-token"}"#,
        )
        .await
        .expect("session file");
        let cache = responses::ValidationCache {
            validated_at: chrono::Utc::now().timestamp(),
            account_data: make_response(2, true, false, true),
        };
        let cache_json = serde_json::to_vec(&cache).expect("cache json");
        tokio::fs::write(
            validation_cache_file_path(cookies.path(), apple_id, "com"),
            cache_json,
        )
        .await
        .expect("validation cache");

        let password_called = Arc::new(AtomicBool::new(false));
        let called_for_provider = Arc::clone(&password_called);
        let provider: crate::password::PasswordProvider = Arc::new(move || {
            called_for_provider.store(true, Ordering::SeqCst);
            Some(SecretString::from("should-not-be-read"))
        });

        let err = send_2fa_push_inner(cookies.path(), apple_id, &provider, "com", &endpoints, None)
            .await
            .expect_err("validated session should not need a 2FA push");

        assert!(
            err.to_string().contains("already authenticated"),
            "unexpected error: {err:#}"
        );
        assert!(
            !password_called.load(Ordering::SeqCst),
            "fresh validation cache must not fall through to SRP"
        );
    }

    #[tokio::test]
    async fn send_2fa_push_treats_live_validate_success_as_authenticated() {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("POST"))
            .and(path("/setup/ws/1/validate"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(2, true, false, true)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let endpoints = Endpoints::for_test_base(&server.uri());
        let cookies = tempfile::tempdir().expect("tempdir");
        let apple_id = "live-get-code@example.com";
        tokio::fs::write(
            session_file_path(cookies.path(), apple_id, "com"),
            r#"{"session_token":"valid-token"}"#,
        )
        .await
        .expect("session file");

        let password_called = Arc::new(AtomicBool::new(false));
        let called_for_provider = Arc::clone(&password_called);
        let provider: crate::password::PasswordProvider = Arc::new(move || {
            called_for_provider.store(true, Ordering::SeqCst);
            Some(SecretString::from("should-not-be-read"))
        });

        let err = send_2fa_push_inner(cookies.path(), apple_id, &provider, "com", &endpoints, None)
            .await
            .expect_err("validated session should not need a 2FA push");

        assert!(
            err.to_string().contains("already authenticated"),
            "unexpected error: {err:#}"
        );
        assert!(
            !password_called.load(Ordering::SeqCst),
            "valid existing session must not fall through to SRP"
        );

        let session = Session::new(cookies.path(), apple_id, &server.uri(), Some(5))
            .await
            .expect("session");
        let cached = session
            .load_validation_cache(responses::VALIDATION_CACHE_GRACE_SECS)
            .await
            .expect("validate success should refresh the cache");
        assert!(
            check_requires_2fa(&cached),
            "regression guard must cover HSA flags that would otherwise demand 2FA"
        );
    }

    async fn mount_persisted_two_factor_push_forbidden(server: &wiremock::MockServer) {
        Mock::given(method("POST"))
            .and(path("/setup/ws/1/validate"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/setup/ws/1/accountLogin"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(2, true, false, true)),
            )
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn get_code_maps_persisted_push_403_to_clean_retry() {
        let server = crate::start_wiremock_or_skip!();
        mount_persisted_two_factor_push_forbidden(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let cookies = tempfile::tempdir().expect("tempdir");
        let apple_id = "get-code-retry@example.com";
        tokio::fs::write(
            session_file_path(cookies.path(), apple_id, "com"),
            r#"{"session_token":"stale-token"}"#,
        )
        .await
        .expect("session file");
        let provider: crate::password::PasswordProvider =
            Arc::new(|| Some(SecretString::from("unused")));

        let err = send_2fa_push_inner(cookies.path(), apple_id, &provider, "com", &endpoints, None)
            .await
            .expect_err("persisted push 403");

        assert!(err.downcast_ref::<RetryWithCleanAuth>().is_some());
    }

    #[tokio::test]
    async fn interactive_login_maps_persisted_push_403_to_clean_retry() {
        let server = crate::start_wiremock_or_skip!();
        mount_persisted_two_factor_push_forbidden(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let cookies = tempfile::tempdir().expect("tempdir");
        let apple_id = "interactive-retry@example.com";
        tokio::fs::write(
            session_file_path(cookies.path(), apple_id, "com"),
            r#"{"session_token":"stale-token"}"#,
        )
        .await
        .expect("session file");
        let session = Session::new(cookies.path(), apple_id, &server.uri(), Some(5))
            .await
            .expect("session");
        let provider: crate::password::PasswordProvider =
            Arc::new(|| Some(SecretString::from("unused")));

        let err = authenticate_inner(
            session,
            &endpoints,
            apple_id,
            &provider,
            "com",
            None,
            None,
            crate::personality::Mode::Off,
            crate::InputMode::Interactive,
        )
        .await
        .expect_err("persisted push 403");

        assert!(err.downcast_ref::<RetryWithCleanAuth>().is_some());
    }

    #[tokio::test]
    async fn live_validate_success_uses_existing_session_even_with_hsa_flags() {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("POST"))
            .and(path("/setup/ws/1/validate"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(2, true, false, true)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cookies = tempfile::tempdir().expect("tempdir");
        let mut session = Session::new(
            cookies.path(),
            "validated@example.com",
            &server.uri(),
            Some(5),
        )
        .await
        .expect("session");
        session
            .session_data
            .insert("session_token".into(), "valid-token".into());

        let password_called = Arc::new(AtomicBool::new(false));
        let called_for_provider = Arc::clone(&password_called);
        let provider: crate::password::PasswordProvider = Arc::new(move || {
            called_for_provider.store(true, Ordering::SeqCst);
            Some(SecretString::from("should-not-be-read"))
        });
        let endpoints = Endpoints::for_test_base(&server.uri());

        let result = authenticate_inner(
            session,
            &endpoints,
            "validated@example.com",
            &provider,
            "com",
            None,
            None,
            crate::personality::Mode::Off,
            crate::InputMode::Interactive,
        )
        .await
        .expect("live validate success should authenticate the persisted session");

        assert!(
            !result.requires_2fa,
            "successful /validate proves the existing session is usable"
        );
        assert!(
            !password_called.load(Ordering::SeqCst),
            "valid existing session must not fall through to SRP"
        );
        let cached = result
            .session
            .load_validation_cache(responses::VALIDATION_CACHE_GRACE_SECS)
            .await
            .expect("validate success should refresh the cache");
        assert!(
            check_requires_2fa(&cached),
            "regression guard must cover HSA flags that would otherwise demand 2FA"
        );
    }

    #[tokio::test]
    async fn recently_validated_requires_session_token_and_fresh_cache() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let mut session = Session::new(
            cookies.path(),
            "cached@example.com",
            "https://setup.icloud.com",
            None,
        )
        .await
        .expect("session");

        session
            .save_validation_cache(&make_response(2, false, true, true))
            .await;
        assert!(
            cached_validation(&session).await.is_none(),
            "cache without a session token must not authenticate an empty session"
        );

        session
            .session_data
            .insert("session_token".into(), "token".into());
        assert!(
            cached_validation(&session).await.is_some(),
            "fresh validation cache should suppress idle re-validation"
        );
    }

    #[tokio::test]
    async fn auth_result_debug_redacts_session_and_data() {
        let cookies = tempfile::tempdir().expect("tempdir");
        let session = Session::new(
            cookies.path(),
            "debug@example.com",
            "https://setup.icloud.com",
            None,
        )
        .await
        .expect("session");
        let result = AuthResult {
            session,
            data: make_response(2, false, true, true),
            requires_2fa: false,
        };

        let rendered = format!("{result:?}");
        assert!(rendered.contains("session: \"<redacted>\""));
        assert!(rendered.contains("data: \"<...>\""));
        assert!(!rendered.contains("debug@example.com"));
    }
}
