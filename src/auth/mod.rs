//! iCloud authentication via Apple's SRP-6a variant with optional 2FA.
//!
//! The flow mirrors Python icloudpd's `PyiCloudService` authentication:
//! session token validation → SRP login → 2FA challenge → session trust.

pub mod endpoints;
pub mod error;
pub mod responses;
pub mod session;
pub mod srp;
pub mod twofa;

use std::path::Path;

use anyhow::Result;
use uuid::Uuid;

use self::endpoints::Endpoints;
use self::error::AuthError;
pub use self::responses::AccountLoginResponse;
use self::session::Session;
pub use self::session::SharedSession;

/// Result of a successful authentication, including the account data payload.
pub struct AuthResult {
    pub session: Session,
    pub data: AccountLoginResponse,
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
/// 4. Checks if 2FA is required; if so, prompts the user.
/// 5. Returns the authenticated session and account data.
pub async fn authenticate(
    cookie_dir: &Path,
    apple_id: &str,
    password_provider: &dyn Fn() -> Option<String>,
    domain: &str,
    client_id: Option<String>,
    timeout_secs: Option<u64>,
) -> Result<AuthResult> {
    let endpoints = Endpoints::for_domain(domain)?;

    let mut session = Session::new(cookie_dir, apple_id, endpoints.home, timeout_secs).await?;

    // Prefer persisted client_id to maintain session continuity across runs
    let client_id = session
        .client_id()
        .cloned()
        .or(client_id)
        .unwrap_or_else(|| format!("auth-{}", Uuid::new_v4()));
    session.set_client_id(&client_id);

    let mut data: Option<AccountLoginResponse> = None;
    if session.session_data.contains_key("session_token") {
        tracing::debug!("Checking session token validity");
        match twofa::validate_token(&mut session, &endpoints).await {
            Ok(d) => {
                tracing::debug!("Existing session token is valid");
                data = Some(d);
            }
            Err(e) => {
                tracing::debug!(
                    "Invalid authentication token, will log in from scratch: {}",
                    e
                );
            }
        }
    }

    if data.is_none() {
        let password = password_provider()
            .ok_or_else(|| AuthError::FailedLogin("Password provider returned no data".into()))?;

        tracing::debug!("Authenticating as {}", apple_id);

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

        let verified =
            twofa::request_2fa_code(&mut session, &endpoints, &client_id, domain).await?;
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
        });
    }

    tracing::info!("Authentication completed successfully");
    Ok(AuthResult { session, data })
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
}
