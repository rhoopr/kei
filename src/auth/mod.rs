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

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Result;
use uuid::Uuid;

use self::endpoints::Endpoints;
use self::error::AuthError;
pub use self::responses::AccountLoginResponse;
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
    password_provider: &dyn Fn() -> Option<String>,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
    code: Option<&str>,
) -> Result<AuthResult> {
    let endpoints = Endpoints::for_domain(domain)?;

    let mut session = Session::new(cookie_dir, apple_id, endpoints.home, timeout_secs).await?;

    // Prefer persisted client_id to maintain session continuity across runs
    let client_id = session
        .session_data
        .client_id
        .clone()
        .or(client_id)
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.session_data.client_id = Some(client_id.clone());

    let mut data: Option<AccountLoginResponse> = None;
    if session.session_data.session_token.is_some() {
        tracing::debug!("Checking session token validity");
        match twofa::validate_token(&mut session, &endpoints).await {
            Ok(d) => {
                tracing::debug!("Existing session token is valid");
                data = Some(d);
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "Invalid authentication token, will log in from scratch"
                );
            }
        }
    }

    if data.is_none() {
        let password = password_provider()
            .ok_or_else(|| AuthError::FailedLogin("Password provider returned no data".into()))?;

        tracing::debug!(apple_id = %apple_id, "Authenticating");

        srp::authenticate_srp(
            &mut session,
            &endpoints,
            apple_id,
            &password,
            &client_id,
            domain,
        )
        .await?;

        let account_data = twofa::authenticate_with_token(&mut session, &endpoints).await?;
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

        // Interactive (TTY, no code): trigger push before prompting.
        // Skip when code is already provided (submit-code) to avoid
        // sending a new code that invalidates the one being submitted.
        if code.is_none() {
            if let Err(e) =
                twofa::trigger_push_notification(&mut session, &endpoints, &client_id, domain).await
            {
                tracing::warn!(error = %e, "Failed to trigger push notification");
            }
        }

        let verified = if let Some(c) = code {
            // Headless: code provided directly (e.g. submit-code subcommand)
            twofa::submit_2fa_code(&mut session, &endpoints, &client_id, domain, c).await?
        } else {
            // Interactive: prompt on stdin (terminal confirmed above)
            twofa::request_2fa_code(&mut session, &endpoints, &client_id, domain).await?
        };

        if !verified {
            return Err(AuthError::TwoFactorFailed("2FA verification failed".into()).into());
        }

        twofa::trust_session(&mut session, &endpoints, &client_id, domain).await?;
        // Re-authenticate to get fresh account data with 2FA-elevated privileges
        let account_data = twofa::authenticate_with_token(&mut session, &endpoints).await?;

        tracing::info!("Authentication completed successfully");
        return Ok(AuthResult {
            session,
            data: account_data,
            requires_2fa: true,
        });
    }

    tracing::info!("Authentication completed successfully");
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
    password_provider: &dyn Fn() -> Option<String>,
    domain: &str,
) -> Result<()> {
    let endpoints = Endpoints::for_domain(domain)?;
    let mut session = Session::new(cookie_dir, apple_id, endpoints.home, None).await?;

    let client_id = session
        .session_data
        .client_id
        .clone()
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.session_data.client_id = Some(client_id.clone());

    let mut data: Option<AccountLoginResponse> = None;
    if session.session_data.session_token.is_some() {
        if let Ok(d) = twofa::validate_token(&mut session, &endpoints).await {
            data = Some(d);
        }
    }

    if data.is_none() {
        let password = password_provider()
            .ok_or_else(|| AuthError::FailedLogin("Password provider returned no data".into()))?;
        srp::authenticate_srp(
            &mut session,
            &endpoints,
            apple_id,
            &password,
            &client_id,
            domain,
        )
        .await?;
        let account_data = twofa::authenticate_with_token(&mut session, &endpoints).await?;
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
        Err(_) => Ok(false),
    }
}

/// Inspect an HTTP response status and body for Apple-specific error patterns.
///
/// Apple often returns HTTP 200 with an error payload, or uses non-standard
/// status codes (421, 450) that need special handling. `apple_rscd` should be
/// the `X-Apple-I-Rscd` header value from the *current* response (not session
/// state), to avoid stale values from prior requests.
pub(crate) fn parse_auth_error(status: u16, body: &str, apple_rscd: Option<&str>) -> AuthError {
    // X-Apple-I-Rscd == "401" → invalid credentials even on HTTP 200
    if apple_rscd == Some("401") {
        return AuthError::FailedLogin("Invalid username or password".into());
    }

    // JSON body errors take precedence — Apple often returns structured errors
    // with specific codes that override the HTTP status.
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if json.get("hasError").and_then(|v| v.as_bool()) == Some(true) {
            if let Some(errors) = json.get("service_errors").and_then(|v| v.as_array()) {
                let messages: Vec<&str> = errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .collect();
                if !messages.is_empty() {
                    return AuthError::ServiceError(messages.join("; "));
                }
            }
            return AuthError::ServiceError("Unknown service error".into());
        }

        let error_code = json
            .get("reason")
            .or_else(|| json.get("errorCode"))
            .and_then(|v| v.as_str());
        match error_code {
            Some("ZONE_NOT_FOUND" | "AUTHENTICATION_FAILED") => return AuthError::SetupRequired,
            Some("ACCESS_DENIED") => return AuthError::RateLimited,
            _ => {}
        }
    }

    // HTTP status fallback when body has no recognized error pattern
    match status {
        401 => AuthError::FailedLogin("Invalid username or password".into()),
        421 => AuthError::WrongRegion,
        450 => AuthError::AccountLocked(
            "Apple has locked this account. Check your email for details.".into(),
        ),
        503 => AuthError::RateLimited,
        _ => AuthError::ApiError {
            code: status,
            message: body.to_string(),
        },
    }
}

/// Apple's HSA2 (two-step verification v2) requires all three conditions:
/// the account uses HSAv2, the browser isn't trusted yet, and the account
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

    // ── parse_auth_error tests ──────────────────────────────────

    #[test]
    fn parse_error_apple_rscd_401() {
        let err = parse_auth_error(200, "", Some("401"));
        assert!(matches!(err, AuthError::FailedLogin(_)));
        assert!(err.to_string().contains("Invalid username or password"));
    }

    #[test]
    fn parse_error_http_401_failed_login() {
        let err = parse_auth_error(401, "", None);
        assert!(matches!(err, AuthError::FailedLogin(_)));
    }

    #[test]
    fn parse_error_http_421_wrong_region() {
        let err = parse_auth_error(421, "", None);
        assert!(matches!(err, AuthError::WrongRegion));
    }

    #[test]
    fn parse_error_http_450_account_locked() {
        let err = parse_auth_error(450, "", None);
        assert!(matches!(err, AuthError::AccountLocked(_)));
    }

    #[test]
    fn parse_error_http_503_rate_limited() {
        let err = parse_auth_error(503, "", None);
        assert!(matches!(err, AuthError::RateLimited));
    }

    #[test]
    fn parse_error_has_error_with_service_errors() {
        let body = r#"{"hasError":true,"service_errors":[{"message":"Account locked"}]}"#;
        let err = parse_auth_error(200, body, None);
        assert!(matches!(err, AuthError::ServiceError(_)));
        assert!(err.to_string().contains("Account locked"));
    }

    #[test]
    fn parse_error_has_error_no_messages() {
        let body = r#"{"hasError":true}"#;
        let err = parse_auth_error(200, body, None);
        assert!(matches!(err, AuthError::ServiceError(_)));
    }

    #[test]
    fn parse_error_zone_not_found() {
        let body = r#"{"reason":"ZONE_NOT_FOUND"}"#;
        let err = parse_auth_error(400, body, None);
        assert!(matches!(err, AuthError::SetupRequired));
    }

    #[test]
    fn parse_error_authentication_failed_in_body_overrides_401_status() {
        // JSON body error codes take precedence over HTTP status
        let body = r#"{"errorCode":"AUTHENTICATION_FAILED"}"#;
        let err = parse_auth_error(401, body, None);
        assert!(matches!(err, AuthError::SetupRequired));
    }

    #[test]
    fn parse_error_access_denied_reason() {
        let body = r#"{"reason":"ACCESS_DENIED"}"#;
        let err = parse_auth_error(403, body, None);
        assert!(matches!(err, AuthError::RateLimited));
    }

    #[test]
    fn parse_error_access_denied_error_code() {
        let body = r#"{"errorCode":"ACCESS_DENIED"}"#;
        let err = parse_auth_error(403, body, None);
        assert!(matches!(err, AuthError::RateLimited));
    }

    #[test]
    fn parse_error_generic_fallback() {
        let err = parse_auth_error(500, "server error", None);
        assert!(matches!(err, AuthError::ApiError { code: 500, .. }));
    }
}
