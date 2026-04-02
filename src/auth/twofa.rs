use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rand::Rng;
use reqwest::header::HeaderMap;
use uuid::Uuid;

use super::endpoints::Endpoints;
use super::session::Session;
use super::srp::{get_auth_headers, APPLE_WIDGET_KEY};
use crate::auth::error::AuthError;
use crate::auth::responses::AccountLoginResponse;

const TWO_FA_CODE_LENGTH: usize = 6;

/// Trigger a push notification to trusted devices for 2FA code entry.
///
/// Apple requires a POST to `/auth/bridge/step/0` to initiate the push
/// notification flow. Without this, some accounts receive a "website login"
/// email instead of a 2FA code on their trusted devices.
///
/// See: icloud-photos-downloader/icloud_photos_downloader#1327
pub async fn trigger_push_notification(
    session: &mut Session,
    endpoints: &Endpoints,
    client_id: &str,
    domain: &str,
) -> Result<()> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock before UNIX epoch")?
        .as_millis();
    let session_uuid = format!("{}-{}", Uuid::new_v4(), timestamp_ms);

    let mut ptkn_bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut ptkn_bytes);
    let mut ptkn = String::with_capacity(128);
    for b in &ptkn_bytes {
        write!(ptkn, "{b:02x}").expect("writing to String cannot fail");
    }

    let data = serde_json::json!({
        "sessionUUID": session_uuid,
        "ptkn": ptkn,
    });

    let overrides: [(&str, &str); 4] = [
        ("Accept", "application/json, text/plain, */*"),
        ("Content-type", "application/json; charset=utf-8"),
        ("X-Apple-App-Id", APPLE_WIDGET_KEY),
        ("X-Apple-Domain-Id", "3"),
    ];
    let headers = get_auth_headers(domain, client_id, &session.session_data, Some(&overrides))?;

    let url = format!("{}/bridge/step/0", endpoints.auth);
    tracing::debug!(url = %url, "Triggering push notification to trusted devices");

    let response = session
        .post(&url, Some(data.to_string()), Some(headers))
        .await?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to read response body");
            String::new()
        });
        anyhow::bail!("Push notification failed (HTTP {status}): {text}");
    }
    Ok(())
}

/// Check whether a string is a valid 6-digit 2FA code.
fn is_valid_2fa_code(code: &str) -> bool {
    code.len() == TWO_FA_CODE_LENGTH && code.chars().all(|c| c.is_ascii_digit())
}

/// Submit a 6-digit 2FA code to Apple's verification endpoint.
///
/// Sends the code to `/verify/trusteddevice/securitycode`.
/// Returns `true` if verification succeeded, `false` if the code was wrong.
pub async fn submit_2fa_code(
    session: &mut Session,
    endpoints: &Endpoints,
    client_id: &str,
    domain: &str,
    code: &str,
) -> Result<bool> {
    if !is_valid_2fa_code(code) {
        tracing::error!(
            expected_length = TWO_FA_CODE_LENGTH,
            "Invalid 2FA code: must be exactly the specified number of digits"
        );
        return Ok(false);
    }

    let data = serde_json::json!({
        "securityCode": {
            "code": code,
        }
    });

    let accept_override: [(&str, &str); 1] = [("Accept", "application/json")];

    let headers = get_auth_headers(
        domain,
        client_id,
        &session.session_data,
        Some(&accept_override),
    )?;

    let url = format!("{}/verify/trusteddevice/securitycode", endpoints.auth);
    let response = session
        .post(&url, Some(data.to_string()), Some(headers))
        .await?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to read response body");
            String::new()
        });
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

/// Prompt the user for a 6-digit 2FA code from a trusted device, then verify it.
///
/// Interactive wrapper around [`submit_2fa_code`] that reads from stdin.
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

    submit_2fa_code(session, endpoints, client_id, domain, &code).await
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
        let text = response.text().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to read response body");
            String::new()
        });
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
        "accountCountryCode": session.session_data.account_country.clone().unwrap_or_default(),
        "dsWebAuthToken": session.session_data.session_token.clone().unwrap_or_default(),
        "extended_login": true,
        "trustToken": session.session_data.trust_token.clone().unwrap_or_default(),
    });

    let url = format!("{}/accountLogin", endpoints.setup);
    let response = session.post(&url, Some(data.to_string()), None).await?;

    let status = response.status();
    // Grab apple_rscd from this specific response (captured by session.post →
    // extract_and_save) before consuming the response body.
    let apple_rscd = session.session_data.apple_rscd.clone();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to read response body");
            String::new()
        });
        return Err(
            crate::auth::parse_auth_error(status.as_u16(), &text, apple_rscd.as_deref()).into(),
        );
    }
    // Apple may return HTTP 200 but signal an error via the X-Apple-I-Rscd header.
    if apple_rscd.as_deref() == Some("401") {
        return Err(AuthError::FailedLogin("Invalid username or password".into()).into());
    }

    let body: AccountLoginResponse = response
        .json()
        .await
        .context("Failed to parse accountLogin response as JSON")?;

    // Apple redirects China mainland accounts to .com.cn — users must
    // re-run with --domain cn to use the correct regional endpoint.
    if let Some(domain_to_use) = &body.domain_to_use {
        return Err(anyhow::anyhow!(
            "Apple insists on using {domain_to_use} for your request. Please use --domain parameter"
        ));
    }

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::session::Session;
    use std::path::PathBuf;

    async fn test_session(name: &str) -> Session {
        let dir = PathBuf::from("/tmp/claude/twofa_tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Session::new(&dir, "test@example.com", "https://example.com", None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_too_short() {
        let mut session = test_session("short").await;
        let endpoints = Endpoints::for_domain("com").unwrap();
        let result = submit_2fa_code(&mut session, &endpoints, "client", "com", "123").await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_too_long() {
        let mut session = test_session("long").await;
        let endpoints = Endpoints::for_domain("com").unwrap();
        let result = submit_2fa_code(&mut session, &endpoints, "client", "com", "1234567").await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_non_digits() {
        let mut session = test_session("nondigit").await;
        let endpoints = Endpoints::for_domain("com").unwrap();
        let result = submit_2fa_code(&mut session, &endpoints, "client", "com", "12345a").await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_empty() {
        let mut session = test_session("empty").await;
        let endpoints = Endpoints::for_domain("com").unwrap();
        let result = submit_2fa_code(&mut session, &endpoints, "client", "com", "").await;
        assert!(!result.unwrap());
    }

    #[test]
    fn test_is_valid_2fa_code_accepts_valid() {
        assert!(is_valid_2fa_code("123456"));
    }

    #[test]
    fn test_is_valid_2fa_code_accepts_leading_zeros() {
        assert!(is_valid_2fa_code("000000"));
    }
}
