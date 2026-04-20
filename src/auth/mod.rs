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

/// Retry budget for Apple's auth endpoints (SRP init/complete, 2FA push,
/// 2FA submit). The flow is user-blocking, so we keep this short: three
/// tries total, short backoffs, capped by `Retry-After`.
pub(crate) const AUTH_RETRY_CONFIG: RetryConfig = RetryConfig {
    max_retries: 2,
    base_delay_secs: 2,
    max_delay_secs: 30,
};

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Result;
use secrecy::{ExposeSecret, SecretString};
use uuid::Uuid;

use self::endpoints::Endpoints;
use self::error::AuthError;
pub use self::responses::AccountLoginResponse;
pub(crate) use self::session::strip_session_routing_state;
use self::session::Session;
pub use self::session::SharedSession;

/// Path to the session data file for a given user, without needing a `Session`.
pub fn session_file_path(cookie_dir: &Path, apple_id: &str) -> PathBuf {
    let sanitized = session::sanitize_username(apple_id);
    cookie_dir.join(format!("{sanitized}.session"))
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

/// Top-level authentication orchestrator.
///
/// 1. Tries to validate the existing session token.
/// 2. If invalid, obtains a password and performs SRP authentication.
/// 3. Authenticates with the resulting token.
/// 4. Checks if 2FA is required; if `code` is `Some`, submits it directly,
///    otherwise prompts the user interactively.
/// 5. Returns the authenticated session and account data.
///
/// When `code` is `None` and 2FA is required but stdin is not a TTY,
/// returns `AuthError::TwoFactorRequired` so the caller can handle it
/// (e.g., fire a notification script and wait).
pub async fn authenticate(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &dyn Fn() -> Option<SecretString>,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
    code: Option<&str>,
) -> Result<AuthResult> {
    let endpoints = Endpoints::for_domain(domain)?;
    let session = Session::new(cookie_dir, apple_id, endpoints.home, timeout_secs).await?;
    authenticate_inner(
        session,
        &endpoints,
        apple_id,
        password_provider,
        domain,
        client_id,
        code,
    )
    .await
}

async fn authenticate_inner(
    mut session: Session,
    endpoints: &Endpoints,
    apple_id: &str,
    password_provider: &dyn Fn() -> Option<SecretString>,
    domain: &str,
    client_id: Option<String>,
    code: Option<&str>,
) -> Result<AuthResult> {
    // Prefer persisted client_id to maintain session continuity across runs
    let client_id = session
        .client_id()
        .map(str::to_owned)
        .or(client_id)
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.set_client_id(&client_id);

    let mut data: Option<AccountLoginResponse> = None;
    let has_session_token = session.session_data.contains_key("session_token");

    // Fast path: if we validated recently, skip the Apple /validate call entirely.
    // The cookies and session token are still in the session file; if they've
    // actually gone stale, the first CloudKit call will 421 and trigger re-auth.
    if has_session_token && code.is_none() {
        if let Some(cached) = session
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
                session.save_validation_cache(&d).await;
                data = Some(d);
            }
            Err(e) => {
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_transient_apple_failure)
                {
                    return Err(e.context(
                        "Apple's auth service is returning transient errors (HTTP 429/5xx). \
                         Wait a few minutes and retry",
                    ));
                }
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_misdirected_request)
                {
                    tracing::warn!(
                        error = %e,
                        "validate returned 421 Misdirected Request; resetting HTTP pool \
                         before accountLogin/SRP"
                    );
                    session.reset_http_clients()?;
                    pool_reset = true;
                } else {
                    tracing::debug!(
                        error = %e,
                        "Invalid authentication token, will log in from scratch"
                    );
                }
            }
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
            Err(e) => {
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_transient_apple_failure)
                {
                    return Err(e.context(
                        "Apple's auth service is returning transient errors (HTTP 429/5xx). \
                         Wait a few minutes and retry",
                    ));
                }
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_misdirected_request)
                {
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
                } else {
                    tracing::debug!(
                        error = %e,
                        "accountLogin failed, falling back to SRP"
                    );
                }
            }
        }
    }

    // If validate and accountLogin both failed (including persistent 421),
    // fall through to SRP. SRP is the canonical path for re-minting session
    // cookies, and trust_token is preserved across the session (via
    // `strip_session_routing_state`) so 2FA is skipped in the common case.
    if data.is_none() {
        let password = password_provider().ok_or_else(|| {
            AuthError::FailedLogin("No password available (see error above for details)".into())
        })?;

        tracing::debug!(apple_id = %apple_id, "Authenticating");

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
            Err(e)
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_misdirected_request) =>
            {
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

    let data = data.ok_or_else(|| anyhow::anyhow!("Authentication produced no account data"))?;

    let requires_2fa = check_requires_2fa(&data);
    if requires_2fa {
        tracing::info!("Two-factor authentication is required");

        // Headless with no code: bail without any Apple API calls.
        // The user triggers the push manually via `get-code`.
        if code.is_none() && !std::io::stdin().is_terminal() {
            return Err(AuthError::TwoFactorRequired.into());
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
            if let Err(e) =
                twofa::trigger_push_notification(&mut session, endpoints, &client_id, domain).await
            {
                tracing::warn!(error = %e, "Failed to trigger push notification");
            }

            const MAX_WRONG_CODES: u32 = 3;
            let mut wrong_codes = 0u32;
            let mut verified = false;
            loop {
                let input = twofa::prompt_2fa_code().await?;
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
    password_provider: &dyn Fn() -> Option<SecretString>,
    domain: &str,
) -> Result<()> {
    let endpoints = Endpoints::for_domain(domain)?;
    let mut session = Session::new(cookie_dir, apple_id, endpoints.home, None).await?;

    let client_id = session
        .client_id()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.set_client_id(&client_id);

    let mut data: Option<AccountLoginResponse> = None;
    let has_session_token = session.session_data.contains_key("session_token");

    if has_session_token {
        if let Some(cached) = session
            .load_validation_cache(responses::VALIDATION_CACHE_GRACE_SECS)
            .await
        {
            data = Some(cached);
        }
    }

    let mut pool_reset = false;
    if data.is_none() && has_session_token {
        match twofa::validate_token(&mut session, &endpoints).await {
            Ok(d) => {
                session.save_validation_cache(&d).await;
                data = Some(d);
            }
            Err(e) => {
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_transient_apple_failure)
                {
                    return Err(e.context(
                        "Apple's auth service is returning transient errors (HTTP 429/5xx). \
                         Wait a few minutes and retry",
                    ));
                }
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_misdirected_request)
                {
                    tracing::warn!(
                        error = %e,
                        "validate returned 421 Misdirected Request; resetting HTTP pool \
                         before accountLogin/SRP"
                    );
                    session.reset_http_clients()?;
                    pool_reset = true;
                }
            }
        }
    }

    // Try accountLogin before SRP (same rationale as authenticate_inner:
    // validate_token is strict, accountLogin is lenient).
    if data.is_none() && has_session_token {
        match twofa::authenticate_with_token(&mut session, &endpoints).await {
            Ok(d) => {
                data = Some(d);
            }
            Err(e)
                if !pool_reset
                    && e.downcast_ref::<AuthError>()
                        .is_some_and(AuthError::is_misdirected_request) =>
            {
                tracing::warn!(
                    error = %e,
                    "accountLogin returned 421 Misdirected Request; resetting HTTP pool \
                     before SRP"
                );
                session.reset_http_clients()?;
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "accountLogin failed during send_2fa_push, falling back to SRP"
                );
            }
        }
    }

    if data.is_none() {
        let password = password_provider().ok_or_else(|| {
            AuthError::FailedLogin("No password available (see error above for details)".into())
        })?;
        srp::authenticate_srp(
            &mut session,
            &endpoints,
            apple_id,
            password.expose_secret(),
            &client_id,
            domain,
        )
        .await?;
        let account_data = match twofa::authenticate_with_token(&mut session, &endpoints).await {
            Ok(d) => d,
            Err(e)
                if e.downcast_ref::<AuthError>()
                    .is_some_and(AuthError::is_misdirected_request) =>
            {
                tracing::warn!(
                    error = %e,
                    "accountLogin returned 421 Misdirected Request after SRP; \
                     resetting HTTP pool and retrying once"
                );
                session.reset_http_clients()?;
                twofa::authenticate_with_token(&mut session, &endpoints).await?
            }
            Err(e) => return Err(e),
        };
        data = Some(account_data);
    }

    let data = data.ok_or_else(|| anyhow::anyhow!("Authentication produced no account data"))?;

    if !check_requires_2fa(&data) {
        anyhow::bail!("Session is already authenticated, 2FA is not required");
    }

    twofa::trigger_push_notification(&mut session, &endpoints, &client_id, domain).await
}

/// Check if the current session token is still valid by calling Apple's
/// validate endpoint. Returns `true` if valid, `false` if expired.
pub async fn validate_session(session: &mut Session, domain: &str) -> Result<bool> {
    let endpoints = Endpoints::for_domain(domain)?;
    match twofa::validate_token(session, &endpoints).await {
        Ok(_) => Ok(true),
        Err(_) => {
            // /validate is strict; try /accountLogin as a lenient fallback.
            // A session is valid if accountLogin succeeds and 2FA is not required
            // (i.e. the trust token is still accepted).
            match twofa::authenticate_with_token(session, &endpoints).await {
                Ok(d) => {
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
                    if let (Some(fresh), Some(stored)) = (fresh_url, stored_url) {
                        if fresh != stored {
                            tracing::info!(
                                old_url = %stored,
                                new_url = %fresh,
                                "CloudKit partition changed, forcing full re-auth"
                            );
                            return Ok(false);
                        }
                    }
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        }
    }
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
}
