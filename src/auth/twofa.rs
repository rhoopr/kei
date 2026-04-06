use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rand::Rng;
use reqwest::header::HeaderMap;
use reqwest::Response;
use serde_json::Value;
use uuid::Uuid;

use super::endpoints::Endpoints;
use super::session::Session;
use super::srp::{get_auth_headers, APPLE_WIDGET_KEY};
use crate::auth::error::AuthError;
use crate::auth::responses::AccountLoginResponse;

const TWO_FA_CODE_LENGTH: usize = 6;

/// Check if the `X-Apple-I-Rscd` response header indicates an authentication
/// failure. Apple sometimes returns HTTP 200 but sets this header to the "real"
/// status code (e.g. 401, 403, 421).
fn check_apple_rscd(response: &Response) -> Option<u16> {
    response
        .headers()
        .get("X-Apple-I-Rscd")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&code| code == 401 || code == 403 || code == 421)
}

/// If `X-Apple-I-Rscd` indicates an auth failure, consume the response body
/// and return a `ServiceError`. Otherwise return `Ok(response)` unchanged.
async fn reject_on_rscd(response: Response) -> Result<Response, AuthError> {
    if let Some(rscd) = check_apple_rscd(&response) {
        let text = response.text().await.unwrap_or_default();
        tracing::debug!(rscd, "Apple rejected session via rscd header");
        return Err(AuthError::ServiceError {
            code: format!("rscd_{rscd}"),
            message: format!("Apple rejected the session (response code {rscd}): {text}"),
        });
    }
    Ok(response)
}

/// Inspect a JSON response body for Apple's error indicators.
///
/// Apple auth APIs sometimes return HTTP 200 with `hasError: true` and/or
/// a `service_errors` array containing error details. This function detects
/// those cases for endpoints whose responses aren't typed structs.
fn check_apple_service_errors(body: &Value) -> Result<(), AuthError> {
    let has_error = body
        .get("hasError")
        .or_else(|| body.get("has_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let errors = body
        .get("service_errors")
        .or_else(|| body.get("serviceErrors"))
        .and_then(Value::as_array);

    if let Some(errors) = errors {
        if let Some(first) = errors.first() {
            let code = first
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let raw_message = first
                .get("message")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .or_else(|| first.get("title").and_then(Value::as_str))
                .unwrap_or("Apple reported an error");
            return Err(AuthError::service_error(code, raw_message));
        }
    }

    if has_error {
        return Err(AuthError::ServiceError {
            code: "unknown".to_string(),
            message: "Apple reported an error but provided no details".to_string(),
        });
    }

    Ok(())
}

/// Classify an HTTP error status from an auth endpoint into a typed `AuthError`.
///
/// - 421/450 → `ServiceError` ("Authentication required")
/// - 5xx → `ServiceError` (server error with context)
/// - anything else → calls `fallback` to produce the default error
fn classify_auth_http_error(
    status: u16,
    text: &str,
    context: &str,
    fallback: impl FnOnce() -> AuthError,
) -> AuthError {
    match status {
        421 | 450 => AuthError::ServiceError {
            code: format!("http_{status}"),
            message: "Authentication required for this account. Please re-authenticate.".into(),
        },
        s if s >= 500 => AuthError::ServiceError {
            code: format!("http_{s}"),
            message: format!("Apple server error during {context} (HTTP {s}): {text}"),
        },
        _ => fallback(),
    }
}

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
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("Push notification failed (HTTP {status}): {text}");
    }

    // Apple can return HTTP 200 with error indicators in the body
    let text = response.text().await.unwrap_or_default();
    if let Ok(body) = serde_json::from_str::<Value>(&text) {
        check_apple_service_errors(&body)?;
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

    // Apple can return HTTP 200 with error indicators in the body
    let text = response.text().await.unwrap_or_default();
    if let Ok(body) = serde_json::from_str::<Value>(&text) {
        if let Err(e) = check_apple_service_errors(&body) {
            tracing::error!(error = %e, "2FA verification returned service error");
            return Ok(false);
        }
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
        let text = response.text().await.unwrap_or_default();
        return Err(
            classify_auth_http_error(status.as_u16(), &text, "validation", || {
                tracing::debug!("Invalid authentication token");
                AuthError::InvalidToken(text.clone())
            })
            .into(),
        );
    }

    let response = reject_on_rscd(response).await?;

    tracing::debug!("Session token is still valid");
    let text = response.text().await.unwrap_or_default();
    let data: AccountLoginResponse = serde_json::from_str(&text).with_context(|| {
        let mut n = text.len().min(200);
        while n > 0 && !text.is_char_boundary(n) {
            n -= 1;
        }
        format!("Validate: expected JSON but got: {:?}", &text[..n])
    })?;
    data.check_errors()?;
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
            classify_auth_http_error(status.as_u16(), &text, "login", || {
                AuthError::FailedLogin(format!("Invalid authentication token: {text}"))
            })
            .into(),
        );
    }

    let response = reject_on_rscd(response).await?;

    let text = response.text().await.unwrap_or_default();
    let body: AccountLoginResponse = serde_json::from_str(&text).with_context(|| {
        let mut n = text.len().min(200);
        while n > 0 && !text.is_char_boundary(n) {
            n -= 1;
        }
        format!("Account login: expected JSON but got: {:?}", &text[..n])
    })?;

    // Check for body-level error indicators before proceeding
    body.check_errors()?;

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
    use tempfile::TempDir;

    async fn test_session() -> (TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path(), "test@example.com", "https://example.com", None)
            .await
            .unwrap();
        (dir, session)
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_too_short() {
        let (_dir, mut session) = test_session().await;
        let endpoints = Endpoints::for_domain("com").unwrap();
        let result = submit_2fa_code(&mut session, &endpoints, "client", "com", "123").await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_too_long() {
        let (_dir, mut session) = test_session().await;
        let endpoints = Endpoints::for_domain("com").unwrap();
        let result = submit_2fa_code(&mut session, &endpoints, "client", "com", "1234567").await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_non_digits() {
        let (_dir, mut session) = test_session().await;
        let endpoints = Endpoints::for_domain("com").unwrap();
        let result = submit_2fa_code(&mut session, &endpoints, "client", "com", "12345a").await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn submit_2fa_code_rejects_empty() {
        let (_dir, mut session) = test_session().await;
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

    #[test]
    fn test_check_apple_service_errors_clean_body() {
        let body = serde_json::json!({"status": "ok"});
        assert!(check_apple_service_errors(&body).is_ok());
    }

    #[test]
    fn test_check_apple_service_errors_has_error_camel_case() {
        let body = serde_json::json!({"hasError": true});
        let err = check_apple_service_errors(&body).unwrap_err();
        assert!(err.to_string().contains("Apple reported an error"));
    }

    #[test]
    fn test_check_apple_service_errors_has_error_snake_case() {
        let body = serde_json::json!({"has_error": true});
        assert!(check_apple_service_errors(&body).is_err());
    }

    #[test]
    fn test_check_apple_service_errors_service_errors_array() {
        let body = serde_json::json!({
            "hasError": true,
            "service_errors": [
                {"code": "AUTH-401", "message": "Authentication required"}
            ]
        });
        let err = check_apple_service_errors(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("AUTH-401"));
        assert!(msg.contains("Authentication required"));
    }

    #[test]
    fn test_check_apple_service_errors_camel_case_key() {
        let body = serde_json::json!({
            "serviceErrors": [
                {"code": "ERR-500", "message": "Internal error"}
            ]
        });
        let err = check_apple_service_errors(&body).unwrap_err();
        assert!(err.to_string().contains("ERR-500"));
    }

    #[test]
    fn test_check_apple_service_errors_title_fallback() {
        let body = serde_json::json!({
            "service_errors": [
                {"code": "ERR", "message": "", "title": "Something failed"}
            ]
        });
        let err = check_apple_service_errors(&body).unwrap_err();
        assert!(err.to_string().contains("Something failed"));
    }

    #[test]
    fn test_check_apple_service_errors_empty_array_ok() {
        let body = serde_json::json!({"service_errors": []});
        assert!(check_apple_service_errors(&body).is_ok());
    }

    #[test]
    fn test_check_apple_service_errors_has_error_false_ok() {
        let body = serde_json::json!({"hasError": false});
        assert!(check_apple_service_errors(&body).is_ok());
    }

    #[test]
    fn test_check_apple_rscd_no_header() {
        let response = http::Response::builder().status(200).body("").unwrap();
        let resp = Response::from(response);
        assert!(check_apple_rscd(&resp).is_none());
    }

    #[test]
    fn test_check_apple_rscd_200_ok() {
        let response = http::Response::builder()
            .status(200)
            .header("X-Apple-I-Rscd", "200")
            .body("")
            .unwrap();
        let resp = Response::from(response);
        assert!(check_apple_rscd(&resp).is_none());
    }

    #[test]
    fn test_check_apple_rscd_401() {
        let response = http::Response::builder()
            .status(200)
            .header("X-Apple-I-Rscd", "401")
            .body("")
            .unwrap();
        let resp = Response::from(response);
        assert_eq!(check_apple_rscd(&resp), Some(401));
    }

    #[test]
    fn test_check_apple_rscd_403() {
        let response = http::Response::builder()
            .status(200)
            .header("X-Apple-I-Rscd", "403")
            .body("")
            .unwrap();
        let resp = Response::from(response);
        assert_eq!(check_apple_rscd(&resp), Some(403));
    }

    #[test]
    fn test_check_apple_rscd_421() {
        let response = http::Response::builder()
            .status(200)
            .header("X-Apple-I-Rscd", "421")
            .body("")
            .unwrap();
        let resp = Response::from(response);
        assert_eq!(check_apple_rscd(&resp), Some(421));
    }

    #[test]
    fn test_check_apple_rscd_non_numeric() {
        let response = http::Response::builder()
            .status(200)
            .header("X-Apple-I-Rscd", "not-a-number")
            .body("")
            .unwrap();
        let resp = Response::from(response);
        assert!(check_apple_rscd(&resp).is_none());
    }

    #[test]
    fn test_check_apple_service_errors_zone_not_found_enriched() {
        let body = serde_json::json!({
            "service_errors": [
                {"code": "ZONE_NOT_FOUND", "message": "Zone not found"}
            ]
        });
        let err = check_apple_service_errors(&body).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("icloud.com"),
            "should mention icloud.com: {msg}"
        );
    }

    #[test]
    fn test_check_apple_service_errors_access_denied_enriched() {
        let body = serde_json::json!({
            "service_errors": [
                {"code": "ACCESS_DENIED", "message": "Access denied"}
            ]
        });
        let err = check_apple_service_errors(&body).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("wait a few minutes"),
            "should suggest waiting: {msg}"
        );
    }
}
