use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::HeaderMap;
use reqwest::Response;
use serde_json::Value;

use super::endpoints::Endpoints;
use super::session::Session;
use super::srp::get_auth_headers;
use super::AUTH_RETRY_CONFIG;
use crate::auth::error::AuthError;
use crate::auth::responses::AccountLoginResponse;
use crate::retry::{parse_retry_after_header, RetryConfig};

const TWO_FA_CODE_LENGTH: usize = 6;

/// Check if the `X-Apple-I-Rscd` header indicates an authentication failure.
/// Apple sometimes returns HTTP 200 but sets this header to the "real"
/// status code (e.g. 401, 403).
///
/// Only `401` and `403` values are acted on. `421` has been observed in the
/// wild but is handled at the HTTP-status layer in `auth::authenticate`
/// (via `AuthError::is_misdirected_request`), so we ignore it here to avoid
/// mis-routing through the session-rejection path.
pub(crate) fn check_rscd_from_headers(headers: &reqwest::header::HeaderMap) -> Option<u16> {
    headers
        .get("X-Apple-I-Rscd")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&code| code == 401 || code == 403)
}

fn check_apple_rscd(response: &Response) -> Option<u16> {
    check_rscd_from_headers(response.headers())
}

/// Build the `ServiceError` that corresponds to an Apple rscd rejection.
pub(crate) fn rscd_service_error(rscd: u16, body: &str) -> AuthError {
    tracing::debug!(rscd, "Apple rejected session via rscd header");
    AuthError::ServiceError {
        code: format!("rscd_{rscd}"),
        message: format!("Apple rejected the session (response code {rscd}): {body}"),
    }
}

/// If `X-Apple-I-Rscd` indicates an auth failure, consume the response body
/// and return a `ServiceError`. Otherwise return `Ok(response)` unchanged.
async fn reject_on_rscd(response: Response) -> Result<Response, AuthError> {
    if let Some(rscd) = check_apple_rscd(&response) {
        let text = response.text().await.unwrap_or_default();
        return Err(rscd_service_error(rscd, &text));
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
/// - 421 → `ServiceError` (Misdirected Request — HTTP/2 routing issue, not auth)
/// - 450 → `ServiceError` (Authentication required)
/// - 5xx → `ServiceError` (server error with context)
/// - anything else → calls `fallback` to produce the default error
fn classify_auth_http_error(
    status: u16,
    text: &str,
    context: &str,
    fallback: impl FnOnce() -> AuthError,
) -> AuthError {
    match status {
        421 => AuthError::ServiceError {
            code: format!("http_{status}"),
            message: format!(
                "Misdirected Request during {context} (HTTP 421): \
                 connection routed to wrong server. {text}"
            ),
        },
        450 => AuthError::ServiceError {
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

/// True for statuses where retrying is likely to change the outcome (Apple
/// rate limits and transient 5xx).
fn is_transient_auth_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// HTTP verb for the auth-layer retry helper; picks between `session.put`
/// and `session.post` without forcing callers to write two near-identical
/// retry loops.
enum AuthMethod<'a> {
    Put,
    Post { body: &'a str },
}

/// POST/PUT against an Apple auth endpoint, retrying transient 5xx/429 up to
/// `retry_config` and honoring `Retry-After`. On non-transient or
/// budget-exhausted 4xx/5xx the response body is read and returned to the
/// caller as `(status, text)` so it can build a typed error; transport errors
/// propagate as-is (they are rare and non-retryable at this layer).
async fn retry_auth_request(
    session: &mut Session,
    url: &str,
    method: AuthMethod<'_>,
    headers_fn: impl Fn(&Session) -> Result<HeaderMap>,
    retry_config: &RetryConfig,
    log_label: &str,
) -> Result<(reqwest::StatusCode, String)> {
    let max_delay = Duration::from_secs(retry_config.max_delay_secs);
    let total_attempts = retry_config.max_retries.saturating_add(1);
    for attempt in 0..total_attempts {
        let headers = headers_fn(session)?;
        let response = match method {
            AuthMethod::Put => session.put(url, Some(headers)).await?,
            AuthMethod::Post { body } => session.post(url, Some(body), Some(headers)).await?,
        };
        let status = response.status();
        let is_last = attempt + 1 >= total_attempts;
        if status.is_success() || !is_transient_auth_status(status.as_u16()) || is_last {
            let text = response.text().await.unwrap_or_default();
            return Ok((status, text));
        }

        let delay = parse_retry_after_header(response.headers(), max_delay)
            .unwrap_or_else(|| retry_config.delay_for_retry(attempt));
        let _ = response.text().await;
        tracing::warn!(
            attempt = attempt + 1,
            total_attempts,
            status = status.as_u16(),
            retry_delay_secs = delay.as_secs(),
            "{log_label}: transient failure, retrying"
        );
        tokio::time::sleep(delay).await;
    }
    Err(anyhow::anyhow!(
        "{log_label}: retry loop exhausted without a decisive response"
    ))
}

/// Trigger a push notification to trusted devices for 2FA code entry.
///
/// Sends a PUT to `/verify/trusteddevice/securitycode` (no body), which
/// tells Apple to push a 2FA code to the account's trusted devices.
///
/// Apple changed this flow around iOS 26.4 — the older `bridge/step/0`
/// POST endpoint no longer reliably triggers pushes. The PUT endpoint
/// works across both old and new Apple auth flows.
///
/// Retries transient 5xx/429 up to [`AUTH_RETRY_CONFIG`]; honors
/// `Retry-After` when Apple provides one.
///
/// See: icloud-photos-downloader/icloud_photos_downloader#1322
pub async fn trigger_push_notification(
    session: &mut Session,
    endpoints: &Endpoints,
    client_id: &str,
    domain: &str,
) -> Result<()> {
    trigger_push_notification_inner(session, endpoints, client_id, domain, &AUTH_RETRY_CONFIG).await
}

async fn trigger_push_notification_inner(
    session: &mut Session,
    endpoints: &Endpoints,
    client_id: &str,
    domain: &str,
    retry_config: &RetryConfig,
) -> Result<()> {
    let url = format!("{}/verify/trusteddevice/securitycode", endpoints.auth);
    tracing::debug!(url = %url, "Requesting 2FA code via PUT");

    let (status, text) = retry_auth_request(
        session,
        &url,
        AuthMethod::Put,
        |s| {
            get_auth_headers(
                domain,
                client_id,
                &s.session_data,
                Some(&[("Accept", "application/json")]),
            )
        },
        retry_config,
        "2FA push notification",
    )
    .await?;

    if status.is_success() {
        return Ok(());
    }
    Err(AuthError::ApiError {
        code: status.as_u16(),
        message: format!("2FA push notification rejected (HTTP {status}): {text}"),
    }
    .into())
}

/// Strip non-digit characters and check whether the result is a valid 6-digit 2FA code.
/// Accepts "123456", "123 456", "123-456", etc.
fn normalize_2fa_code(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    if digits.len() == TWO_FA_CODE_LENGTH {
        Some(digits)
    } else {
        None
    }
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
    submit_2fa_code_inner(
        session,
        endpoints,
        client_id,
        domain,
        code,
        &AUTH_RETRY_CONFIG,
    )
    .await
}

async fn submit_2fa_code_inner(
    session: &mut Session,
    endpoints: &Endpoints,
    client_id: &str,
    domain: &str,
    code: &str,
    retry_config: &RetryConfig,
) -> Result<bool> {
    let Some(code) = normalize_2fa_code(code) else {
        tracing::error!(
            expected_length = TWO_FA_CODE_LENGTH,
            "Invalid 2FA code: must contain exactly {TWO_FA_CODE_LENGTH} digits"
        );
        return Ok(false);
    };

    let data = serde_json::json!({ "securityCode": { "code": code } });
    let url = format!("{}/verify/trusteddevice/securitycode", endpoints.auth);
    let body = data.to_string();

    let (status, text) = retry_auth_request(
        session,
        &url,
        AuthMethod::Post { body: &body },
        |s| {
            get_auth_headers(
                domain,
                client_id,
                &s.session_data,
                Some(&[("Accept", "application/json")]),
            )
        },
        retry_config,
        "2FA code submit",
    )
    .await?;

    if !status.is_success() {
        // -21669 = incorrect verification code
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
    if let Ok(body) = serde_json::from_str::<Value>(&text) {
        if let Err(e) = check_apple_service_errors(&body) {
            tracing::error!(error = %e, "2FA verification returned service error");
            return Ok(false);
        }
    }

    tracing::debug!("Code verification successful");
    Ok(true)
}

/// Prompt the user for a 2FA code on stdin.
///
/// Returns the trimmed input. An empty string means the user pressed Enter
/// without typing a code (i.e. they want a new code sent to their device).
pub async fn prompt_2fa_code() -> Result<String> {
    Ok(tokio::task::spawn_blocking(|| {
        print!("Enter 2FA code (or press Enter to request a new code): ");
        io::stdout().flush()?;
        let mut code = String::new();
        io::stdin().read_line(&mut code)?;
        Ok::<String, io::Error>(code.trim().to_string())
    })
    .await??)
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
///
/// 421 Misdirected Request surfaces as-is; the caller (`auth::authenticate`)
/// resets the HTTP connection pool once before trying `accountLogin`/SRP.
pub async fn validate_token(
    session: &mut Session,
    endpoints: &Endpoints,
) -> Result<AccountLoginResponse> {
    tracing::debug!("Checking session token validity");

    let mut headers = HeaderMap::new();
    headers.insert("Origin", session.home_endpoint().parse()?);
    headers.insert("Referer", format!("{}/", session.home_endpoint()).parse()?);

    let url = format!("{}/validate", endpoints.setup);
    let response = session.post(&url, Some("null"), Some(headers)).await?;

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
///
/// 421 Misdirected Request surfaces as-is; pool resets happen at the
/// `auth::authenticate` level so the reset amortizes across callers.
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
    let body = data.to_string();
    let response = session.post(&url, Some(&body), None).await?;

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
    fn test_normalize_2fa_code_plain_digits() {
        assert_eq!(normalize_2fa_code("123456").unwrap(), "123456");
    }

    #[test]
    fn test_normalize_2fa_code_with_space() {
        assert_eq!(normalize_2fa_code("123 456").unwrap(), "123456");
    }

    #[test]
    fn test_normalize_2fa_code_with_dash() {
        assert_eq!(normalize_2fa_code("123-456").unwrap(), "123456");
    }

    #[test]
    fn test_normalize_2fa_code_leading_zeros() {
        assert_eq!(normalize_2fa_code("000000").unwrap(), "000000");
    }

    #[test]
    fn test_normalize_2fa_code_too_short() {
        assert!(normalize_2fa_code("12345").is_none());
    }

    #[test]
    fn test_normalize_2fa_code_too_long() {
        assert!(normalize_2fa_code("1234567").is_none());
    }

    #[test]
    fn test_normalize_2fa_code_letters_rejected() {
        assert!(normalize_2fa_code("12345a").is_none());
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
    fn test_check_apple_rscd_421_ignored() {
        // 200 + rscd=421 has not been observed in the wild; only rscd=401/403
        // indicates an auth rejection kei needs to act on.
        let response = http::Response::builder()
            .status(200)
            .header("X-Apple-I-Rscd", "421")
            .body("")
            .unwrap();
        let resp = Response::from(response);
        assert!(check_apple_rscd(&resp).is_none());
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

    #[test]
    fn test_classify_421_is_misdirected_not_auth() {
        let err = classify_auth_http_error(421, "body", "validation", || {
            panic!("fallback should not be called for 421")
        });
        assert!(err.is_misdirected_request());
        let msg = err.to_string();
        assert!(
            msg.contains("Misdirected Request"),
            "421 should say misdirected, got: {msg}"
        );
        assert!(
            !msg.contains("re-authenticate"),
            "421 should not suggest re-auth, got: {msg}"
        );
    }

    #[test]
    fn test_classify_450_requires_auth() {
        let err = classify_auth_http_error(450, "body", "login", || {
            panic!("fallback should not be called for 450")
        });
        assert!(!err.is_misdirected_request());
        let msg = err.to_string();
        assert!(
            msg.contains("re-authenticate"),
            "450 should suggest re-auth, got: {msg}"
        );
    }

    #[test]
    fn test_classify_421_produces_http_421_code() {
        let err =
            classify_auth_http_error(421, "", "validation", || panic!("should not be called"));
        if let AuthError::ServiceError { code, .. } = &err {
            assert_eq!(code, "http_421");
        } else {
            panic!("expected ServiceError, got: {err:?}");
        }
    }

    #[test]
    fn test_classify_5xx_server_error() {
        let err = classify_auth_http_error(503, "Service Unavailable", "validation", || {
            panic!("fallback should not be called for 5xx")
        });
        let msg = err.to_string();
        assert!(msg.contains("503"));
        assert!(msg.contains("server error"));
    }

    #[test]
    fn test_classify_other_uses_fallback() {
        let err = classify_auth_http_error(401, "Unauthorized", "login", || {
            AuthError::InvalidToken("custom fallback".into())
        });
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    // ────────────────────────────────────────────────────────────────
    // wiremock-based tests for 2FA push / submit retry behavior.
    //
    // These tests verify that transient 5xx/429 responses are retried
    // (up to TWOFA_RETRY_CONFIG) rather than bailing immediately with
    // "2FA code request failed (HTTP 429)" and that the final error is
    // a typed AuthError::ApiError rather than an opaque anyhow::bail.
    // ────────────────────────────────────────────────────────────────

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Zero-delay retry config for wiremock tests. Real retry delays (2s+)
    /// would stall the suite; `start_paused` isn't usable because it also
    /// halts the clock that reqwest's connection timeout relies on.
    const TEST_RETRY: RetryConfig = RetryConfig {
        max_retries: 2,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };

    async fn session_for(server: &MockServer) -> (TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path(), "test@example.com", &server.uri(), Some(5))
            .await
            .unwrap();
        (dir, session)
    }

    #[tokio::test]
    async fn trigger_push_retries_on_503_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (_dir, mut session) = session_for(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        trigger_push_notification_inner(&mut session, &endpoints, "client-id", "com", &TEST_RETRY)
            .await
            .expect("should succeed after a 503 retry");
    }

    #[tokio::test]
    async fn trigger_push_retries_on_429_with_retry_after_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "1")
                    .set_body_string("slow down"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (_dir, mut session) = session_for(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        // max_delay_secs=0 in TEST_RETRY caps the Retry-After=1s to 0s, so
        // the retry happens immediately but still structurally honors the
        // header path (parse + clamp).
        trigger_push_notification_inner(&mut session, &endpoints, "client-id", "com", &TEST_RETRY)
            .await
            .expect("429 + Retry-After must retry once and succeed");
    }

    #[tokio::test]
    async fn trigger_push_persistent_429_returns_typed_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;

        let (_dir, mut session) = session_for(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let err = trigger_push_notification_inner(
            &mut session,
            &endpoints,
            "client-id",
            "com",
            &TEST_RETRY,
        )
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().expect("typed AuthError");
        match auth_err {
            AuthError::ApiError { code, .. } => assert_eq!(*code, 429),
            other => panic!("expected ApiError(429), got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn trigger_push_non_transient_4xx_bails_without_retry() {
        // 400 is not a transient auth status, so no retries should happen.
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1) // exactly one call; no retry
            .mount(&server)
            .await;

        let (_dir, mut session) = session_for(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let err = trigger_push_notification_inner(
            &mut session,
            &endpoints,
            "client-id",
            "com",
            &TEST_RETRY,
        )
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().expect("typed AuthError");
        assert!(
            matches!(auth_err, AuthError::ApiError { code: 400, .. }),
            "expected ApiError(400), got: {auth_err:?}"
        );
    }

    #[tokio::test]
    async fn submit_2fa_code_retries_on_503_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let (_dir, mut session) = session_for(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let ok = submit_2fa_code_inner(
            &mut session,
            &endpoints,
            "client-id",
            "com",
            "123456",
            &TEST_RETRY,
        )
        .await
        .expect("submit should succeed after 503 retry");
        assert!(ok, "valid code on second try => verification success");
    }

    #[tokio::test]
    async fn submit_2fa_code_wrong_code_response_does_not_retry() {
        // -21669 is Apple's "wrong code" sentinel. It lives in a 4xx body,
        // but the caller returns Ok(false) rather than retrying.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("{\"service_errors\": [{\"code\":\"-21669\"}]}"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (_dir, mut session) = session_for(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let ok = submit_2fa_code_inner(
            &mut session,
            &endpoints,
            "client-id",
            "com",
            "123456",
            &TEST_RETRY,
        )
        .await
        .expect("wrong-code response should be Ok(false), not an error");
        assert!(!ok, "wrong code must return Ok(false)");
    }

    #[tokio::test]
    async fn submit_2fa_code_persistent_503_returns_typed_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/appleauth/auth/verify/trusteddevice/securitycode"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&server)
            .await;

        let (_dir, mut session) = session_for(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let err = submit_2fa_code_inner(
            &mut session,
            &endpoints,
            "client-id",
            "com",
            "123456",
            &TEST_RETRY,
        )
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().expect("typed AuthError");
        assert!(
            matches!(auth_err, AuthError::ApiError { code: 503, .. }),
            "expected ApiError(503) after retry exhaustion, got: {auth_err:?}"
        );
    }
}
