use std::collections::HashMap;
use std::io::{self, Write};

use anyhow::{Context, Result};
use reqwest::header::HeaderMap;

use super::endpoints::Endpoints;
use super::session::Session;
use super::srp::get_auth_headers;
use crate::auth::error::AuthError;
use crate::auth::responses::AccountLoginResponse;

const TWO_FA_CODE_LENGTH: usize = 6;

/// Prompt the user for a 6-digit 2FA code from a trusted device, then verify it.
///
/// Sends the code to Apple's `/verify/trusteddevice/securitycode` endpoint.
/// Returns `true` if verification succeeded.
pub async fn request_2fa_code(
    session: &mut Session,
    endpoints: &Endpoints,
    client_id: &str,
    domain: &str,
) -> Result<bool> {
    let code = tokio::task::spawn_blocking(|| {
        print!("Please enter the 2FA code from your trusted device: ");
        io::stdout().flush()?;
        let mut code = String::new();
        io::stdin().read_line(&mut code)?;
        Ok::<String, io::Error>(code.trim().to_string())
    })
    .await??;

    if code.len() != TWO_FA_CODE_LENGTH || !code.chars().all(|c| c.is_ascii_digit()) {
        tracing::error!(
            "Invalid 2FA code: must be exactly {} digits",
            TWO_FA_CODE_LENGTH
        );
        return Ok(false);
    }

    let data = serde_json::json!({
        "securityCode": {
            "code": code,
        }
    });

    let mut accept_override = HashMap::new();
    accept_override.insert("Accept".to_string(), "application/json".to_string());

    let headers = get_auth_headers(
        domain,
        client_id,
        &session.session_data,
        Some(accept_override),
    )?;

    let url = format!("{}/verify/trusteddevice/securitycode", endpoints.auth);
    let response = session
        .post(&url, Some(data.to_string()), Some(headers))
        .await?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        // Apple error code -21669 = incorrect verification code
        if text.contains("-21669") {
            tracing::error!("Code verification failed: wrong code");
            return Ok(false);
        }
        return Err(AuthError::ApiError {
            code: status.as_u16(),
            message: text,
        }
        .into());
    }

    tracing::debug!("Code verification successful");
    Ok(true)
}

/// Trust the current session so the user is not prompted for 2FA again.
///
/// GET `{auth_endpoint}/2sv/trust`
pub async fn trust_session(
    session: &mut Session,
    endpoints: &Endpoints,
    client_id: &str,
    domain: &str,
) -> Result<bool> {
    let headers = get_auth_headers(domain, client_id, &session.session_data, None)?;
    let url = format!("{}/2sv/trust", endpoints.auth);

    session
        .get(&url, Some(headers))
        .await
        .context("Failed to trust session")?;
    tracing::debug!("Session trusted successfully");
    Ok(true)
}

/// Validate the current session token.
///
/// POST `{setup_endpoint}/validate` with body "null".
/// Returns the parsed JSON response body on success.
pub async fn validate_token(
    session: &mut Session,
    endpoints: &Endpoints,
) -> Result<AccountLoginResponse> {
    tracing::debug!("Checking session token validity");

    let mut headers = HeaderMap::new();
    headers.insert("Origin", session.home_endpoint().parse()?);
    headers.insert("Referer", format!("{}/", session.home_endpoint()).parse()?);

    let url = format!("{}/validate", endpoints.setup);
    let response = session
        .post(&url, Some("null".to_string()), Some(headers))
        .await?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        tracing::debug!("Invalid authentication token");
        return Err(AuthError::InvalidToken(text).into());
    }

    tracing::debug!("Session token is still valid");
    let data: AccountLoginResponse = response
        .json()
        .await
        .context("Failed to parse validate response as JSON")?;
    Ok(data)
}

/// Authenticate using a session token (dsWebAuthToken).
///
/// POST `{setup_endpoint}/accountLogin` with the token and trust token.
/// Returns the parsed JSON response containing account data.
pub async fn authenticate_with_token(
    session: &mut Session,
    endpoints: &Endpoints,
) -> Result<AccountLoginResponse> {
    let data = serde_json::json!({
        "accountCountryCode": session.session_data.get("account_country").cloned().unwrap_or_default(),
        "dsWebAuthToken": session.session_data.get("session_token").cloned().unwrap_or_default(),
        "extended_login": true,
        "trustToken": session.session_data.get("trust_token").cloned().unwrap_or_default(),
    });

    let url = format!("{}/accountLogin", endpoints.setup);
    let response = session.post(&url, Some(data.to_string()), None).await?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(
            AuthError::FailedLogin(format!("Invalid authentication token: {}", text)).into(),
        );
    }

    let body: AccountLoginResponse = response
        .json()
        .await
        .context("Failed to parse accountLogin response as JSON")?;

    // Apple redirects China mainland accounts to .com.cn — users must
    // re-run with --domain cn to use the correct regional endpoint.
    if let Some(domain_to_use) = &body.domain_to_use {
        return Err(anyhow::anyhow!(
            "Apple insists on using {} for your request. Please use --domain parameter",
            domain_to_use
        ));
    }

    Ok(body)
}
