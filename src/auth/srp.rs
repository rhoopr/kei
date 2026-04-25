use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use num_bigint::BigUint;
use rand::RngExt;
use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};

use std::collections::HashMap;
use std::time::Duration;

use super::endpoints::Endpoints;
use super::session::Session;
use super::twofa::{check_rscd_from_headers, rscd_service_error};
use super::AUTH_RETRY_CONFIG;
use crate::auth::error::AuthError;
use crate::retry::parse_retry_after_header;

/// Buffered HTTP response for SRP authentication steps.
/// Decouples the SRP flow from `reqwest::Response` for testability.
#[derive(Debug)]
pub(crate) struct SrpResponse {
    pub(crate) status: u16,
    body: Vec<u8>,
    pub(crate) headers: HeaderMap,
}

impl SrpResponse {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn is_client_error(&self) -> bool {
        (400..500).contains(&self.status)
    }

    fn is_server_error(&self) -> bool {
        (500..600).contains(&self.status)
    }

    fn json<T: serde::de::DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_slice(&self.body)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Abstracts the HTTP transport used by SRP authentication.
/// Production code uses `Session`; tests inject stub responses.
#[async_trait::async_trait]
pub(crate) trait SrpTransport {
    async fn post(
        &mut self,
        url: &str,
        body: Option<&str>,
        headers: Option<HeaderMap>,
    ) -> Result<SrpResponse>;
    fn session_data(&self) -> &HashMap<String, String>;
}

#[async_trait::async_trait]
impl SrpTransport for Session {
    async fn post(
        &mut self,
        url: &str,
        body: Option<&str>,
        headers: Option<HeaderMap>,
    ) -> Result<SrpResponse> {
        let response = Self::post(self, url, body, headers).await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = response.bytes().await?;
        Ok(SrpResponse {
            status,
            body: bytes.to_vec(),
            headers,
        })
    }

    fn session_data(&self) -> &HashMap<String, String> {
        &self.session_data
    }
}

/// Apple's public OAuth widget key — embedded in icloud.com's JavaScript.
pub(crate) const APPLE_WIDGET_KEY: &str =
    "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";

/// RFC 5054 2048-bit SRP group prime (same as `srp::groups::G_2048`).
const N_HEX: &str = concat!(
    "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC319294",
    "3DB56050A37329CBB4A099ED8193E0757767A13DD52312AB4B03310D",
    "CD7F48A9DA04FD50E8083969EDB767B0CF6095179A163AB3661A05FB",
    "D5FAAAE82918A9962F0B93B855F97993EC975EEAA80D740ADBF4FF74",
    "7359D041D5C33EA71D281E446B14773BCA97B43A23FB801676BD207A",
    "436C6481F1D2B9078717461A5B9D32E688F87748544523B524B0D57D",
    "5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E73",
    "03CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB6",
    "94B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F",
    "9E4AFF73",
);
const G_VAL: u32 = 2;

/// Apple's SRP uses PBKDF2 over a SHA-256 hash of the password, not the
/// raw password. The `s2k_fo` protocol variant hex-encodes the hash first,
/// while `s2k` uses raw bytes — both are PBKDF2'd with the server-provided salt.
///
/// Returns a fixed 32-byte array, avoiding heap allocation.
fn derive_apple_password(password: &str, protocol: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let hash = Sha256::digest(password.as_bytes());

    // For s2k_fo, we need to hex-encode first (64 bytes), then PBKDF2.
    // For s2k, use the raw 32-byte hash directly.
    let mut key = [0u8; 32];
    if protocol == "s2k_fo" {
        use std::fmt::Write;
        let hex_str = hash.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });
        pbkdf2::pbkdf2_hmac::<Sha256>(hex_str.as_bytes(), salt, iterations, &mut key);
    } else {
        pbkdf2::pbkdf2_hmac::<Sha256>(&hash, salt, iterations, &mut key);
    }

    key
}

/// Apple's SRP omits the username from the x computation (unlike standard SRP),
/// but retains the colon separator. See Python's `no_username_in_x()` flag.
fn compute_x(salt: &[u8], password_key: &[u8]) -> BigUint {
    let mut inner_hasher = Sha256::new();
    inner_hasher.update(b":");
    inner_hasher.update(password_key);
    let inner = inner_hasher.finalize();
    let mut outer = Sha256::new();
    outer.update(salt);
    outer.update(inner);
    BigUint::from_bytes_be(&outer.finalize())
}

/// Compute k = H(N | pad(g))  — SRP-6a multiplier.
fn compute_k(n: &BigUint, g: &BigUint) -> BigUint {
    let n_bytes = n.to_bytes_be();
    let g_bytes = g.to_bytes_be();
    let pad_len = n_bytes.len();
    let mut g_padded = vec![0u8; pad_len.saturating_sub(g_bytes.len())];
    g_padded.extend_from_slice(&g_bytes);

    let mut hasher = Sha256::new();
    hasher.update(&n_bytes);
    hasher.update(&g_padded);
    BigUint::from_bytes_be(&hasher.finalize())
}

/// Compute u = H(pad(A) | pad(B)).
fn compute_u(a_pub: &BigUint, b_pub: &BigUint, n: &BigUint) -> BigUint {
    let pad_len = n.to_bytes_be().len();

    let a_bytes = a_pub.to_bytes_be();
    let mut a_padded = vec![0u8; pad_len.saturating_sub(a_bytes.len())];
    a_padded.extend_from_slice(&a_bytes);

    let b_bytes = b_pub.to_bytes_be();
    let mut b_padded = vec![0u8; pad_len.saturating_sub(b_bytes.len())];
    b_padded.extend_from_slice(&b_bytes);

    let mut hasher = Sha256::new();
    hasher.update(&a_padded);
    hasher.update(&b_padded);
    BigUint::from_bytes_be(&hasher.finalize())
}

/// Compute M1 = H(H(N) XOR H(g) | H(username) | salt | A | B | K).
/// Note: `no_username_in_x` only affects x computation, NOT M1.
/// M1 always uses the real username (`apple_id`).
///
/// Returns a fixed 32-byte array, avoiding heap allocation.
fn compute_m1(
    n: &BigUint,
    g: &BigUint,
    username: &[u8],
    salt: &[u8],
    a_pub: &BigUint,
    b_pub: &BigUint,
    key: &[u8],
) -> [u8; 32] {
    let n_bytes = n.to_bytes_be();
    let g_bytes = g.to_bytes_be();
    // RFC 5054: pad g to N's byte length before hashing in HNxorg
    let mut g_padded = vec![0u8; n_bytes.len().saturating_sub(g_bytes.len())];
    g_padded.extend_from_slice(&g_bytes);
    let h_n = Sha256::digest(&n_bytes);
    let h_g = Sha256::digest(&g_padded);
    // XOR the hashes into a fixed array instead of Vec
    let mut h_xor = [0u8; 32];
    for ((out, a), b) in h_xor.iter_mut().zip(h_n.iter()).zip(h_g.iter()) {
        *out = a ^ b;
    }
    let h_username = Sha256::digest(username);

    let mut hasher = Sha256::new();
    hasher.update(h_xor);
    hasher.update(h_username);
    hasher.update(salt);
    hasher.update(a_pub.to_bytes_be());
    hasher.update(b_pub.to_bytes_be());
    hasher.update(key);
    hasher.finalize().into()
}

/// Compute M2 = H(A | M1 | K).
///
/// Returns a fixed 32-byte array, avoiding heap allocation.
fn compute_m2(a_pub: &BigUint, m1: &[u8], key: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(a_pub.to_bytes_be());
    hasher.update(m1);
    hasher.update(key);
    hasher.finalize().into()
}

/// Build the Apple OAuth/auth headers required for SRP authentication requests.
pub(crate) fn get_auth_headers(
    domain: &str,
    client_id: &str,
    session_data: &HashMap<String, String>,
    overrides: Option<&[(&str, &str)]>,
) -> Result<HeaderMap> {
    let redirect_uri = if domain == "cn" {
        "https://www.icloud.com.cn"
    } else {
        "https://www.icloud.com"
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        "Accept",
        HeaderValue::from_static("application/json, text/javascript"),
    );
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    headers.insert(
        "X-Apple-OAuth-Client-Id",
        HeaderValue::from_static(APPLE_WIDGET_KEY),
    );
    headers.insert(
        "X-Apple-OAuth-Client-Type",
        HeaderValue::from_static("firstPartyAuth"),
    );
    headers.insert(
        "X-Apple-OAuth-Redirect-URI",
        HeaderValue::from_str(redirect_uri)?,
    );
    headers.insert(
        "X-Apple-OAuth-Require-Grant-Code",
        HeaderValue::from_static("true"),
    );
    headers.insert(
        "X-Apple-OAuth-Response-Mode",
        HeaderValue::from_static("web_message"),
    );
    headers.insert(
        "X-Apple-OAuth-Response-Type",
        HeaderValue::from_static("code"),
    );
    headers.insert("X-Apple-OAuth-State", HeaderValue::from_str(client_id)?);
    headers.insert(
        "X-Apple-Widget-Key",
        HeaderValue::from_static(APPLE_WIDGET_KEY),
    );

    if let Some(scnt) = session_data.get("scnt") {
        if let Ok(v) = HeaderValue::from_str(scnt) {
            headers.insert("scnt", v);
        }
    }
    if let Some(session_id) = session_data.get("session_id") {
        if let Ok(v) = HeaderValue::from_str(session_id) {
            headers.insert("X-Apple-ID-Session-Id", v);
        }
    }

    if let Some(ovr) = overrides {
        for &(key, val) in ovr {
            if let Ok(v) = HeaderValue::from_str(val) {
                if let Ok(name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                    headers.insert(name, v);
                }
            }
        }
    }

    Ok(headers)
}

/// Perform SRP-6a authentication against Apple's auth servers.
///
/// Uses a custom SRP implementation that matches Apple's variant:
/// - no username in the x computation (Python's `no_username_in_x()`)
/// - PBKDF2-derived password key
pub async fn authenticate_srp(
    transport: &mut impl SrpTransport,
    endpoints: &Endpoints,
    apple_id: &str,
    password: &str,
    client_id: &str,
    domain: &str,
) -> Result<()> {
    let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse SRP prime"))?;
    let g = BigUint::from(G_VAL);

    let mut a_bytes = vec![0u8; 32];
    rand::rng().fill(&mut a_bytes[..]);
    let a_private = BigUint::from_bytes_be(&a_bytes);

    // A = g^a mod N
    let a_pub = g.modpow(&a_private, &n);
    let a_pub_b64 = BASE64.encode(a_pub.to_bytes_be());

    let init_body = serde_json::json!({
        "a": a_pub_b64,
        "accountName": apple_id,
        "protocols": ["s2k", "s2k_fo"],
    });

    let referer = format!("{}/", endpoints.auth_root);
    let overrides: [(&str, &str); 2] = [("Origin", endpoints.auth_root), ("Referer", &referer)];

    let init_headers = get_auth_headers(
        domain,
        client_id,
        transport.session_data(),
        Some(&overrides),
    )?;

    tracing::debug!(apple_id = %apple_id, "Initiating SRP authentication");

    let init_url = format!("{}/signin/init", endpoints.auth);
    let init_body = init_body.to_string();
    // First attempt reuses the pre-computed headers (they include the
    // current scnt/session_id). Retries rebuild headers so any rotated
    // values from a 5xx response are picked up.
    let mut init_attempt_headers = Some(init_headers);
    let response = srp_post(
        transport,
        "init",
        &init_url,
        &init_body,
        &mut init_attempt_headers,
        |sd| get_auth_headers(domain, client_id, sd, Some(&overrides)),
    )
    .await?;

    if let Some(rscd) = check_rscd_from_headers(&response.headers) {
        return Err(rscd_service_error(rscd, &response.text()).into());
    }

    // A 401 at /signin/init means Apple rejected the *session context*
    // (stale scnt/cookies/client-id), not the password — SRP hasn't yet
    // sent the M1 proof. Surface as a typed API error instead of
    // FailedLogin so the caller doesn't tell the user their password is
    // wrong when the real cause is a transient auth-CDN issue.
    if response.status == 401 {
        return Err(AuthError::ApiError {
            code: 401,
            message:
                "SRP init rejected (HTTP 401). Apple's auth session context is stale; retry shortly."
                    .into(),
        }
        .into());
    }
    if !response.is_success() && response.status != 409 {
        let text = response.text();
        let message = if text.contains('<') {
            format!("HTTP {} from Apple auth service", response.status)
        } else {
            text
        };
        return Err(AuthError::ApiError {
            code: response.status,
            message,
        }
        .into());
    }

    let body: super::responses::SrpInitResponse = response.json().with_context(|| {
        let text = response.text();
        let mut n = text.len().min(200);
        while n > 0 && !text.is_char_boundary(n) {
            n -= 1;
        }
        format!(
            "SRP init: expected JSON but got (HTTP {}): {:?}",
            response.status,
            &text[..n]
        )
    })?;

    let iterations = u32::try_from(body.iteration).context("SRP iteration count exceeds u32")?;
    anyhow::ensure!(
        iterations <= 1_000_000,
        "SRP iteration count {iterations} exceeds safety limit"
    );

    let salt = BASE64
        .decode(&body.salt)
        .context("Failed to decode SRP salt")?;
    let b_pub_bytes = BASE64
        .decode(&body.b)
        .context("Failed to decode SRP public key")?;
    let b_pub = BigUint::from_bytes_be(&b_pub_bytes);

    let password_key = derive_apple_password(password, &body.protocol, &salt, iterations);

    tracing::debug!(
        protocol = %body.protocol,
        iterations,
        "SRP parameters"
    );
    let x = compute_x(&salt, &password_key);
    let k = compute_k(&n, &g);
    let u = compute_u(&a_pub, &b_pub, &n);

    if u == BigUint::ZERO {
        return Err(AuthError::FailedLogin("SRP: u is zero, aborting".into()).into());
    }
    if &b_pub % &n == BigUint::ZERO {
        return Err(AuthError::FailedLogin("SRP: B mod N is zero, aborting".into()).into());
    }

    let v = g.modpow(&x, &n);
    let kv = (&k * &v) % &n;
    // BigUint can't go negative, so add N to prevent underflow when B < kv
    let base = if b_pub >= kv {
        &b_pub - &kv
    } else {
        &b_pub + &n - &kv
    };
    let exp = &a_private + &u * &x;
    let s = base.modpow(&exp, &n);

    let key = Sha256::digest(s.to_bytes_be());
    let m1 = compute_m1(&n, &g, apple_id.as_bytes(), &salt, &a_pub, &b_pub, &key);
    let m2 = compute_m2(&a_pub, &m1, &key);

    let m1_b64 = BASE64.encode(m1);
    let m2_b64 = BASE64.encode(m2);

    let trust_tokens: Vec<String> = transport
        .session_data()
        .get("trust_token")
        .filter(|t| !t.is_empty())
        .map(|t| vec![t.clone()])
        .unwrap_or_default();

    let complete_body = serde_json::json!({
        "accountName": apple_id,
        "c": body.c,
        "m1": m1_b64,
        "m2": m2_b64,
        "rememberMe": true,
        "trustTokens": trust_tokens,
    });

    // Rebuild headers — init response may have rotated scnt/session_id
    let referer = format!("{}/", endpoints.auth_root);
    let overrides: [(&str, &str); 2] = [("Origin", endpoints.auth_root), ("Referer", &referer)];
    let complete_headers = get_auth_headers(
        domain,
        client_id,
        transport.session_data(),
        Some(&overrides),
    )?;
    let complete_url = format!(
        "{}/signin/complete?isRememberMeEnabled=true",
        endpoints.auth
    );
    let complete_body = complete_body.to_string();
    let mut complete_attempt_headers = Some(complete_headers);
    let response = srp_post(
        transport,
        "complete",
        &complete_url,
        &complete_body,
        &mut complete_attempt_headers,
        |sd| get_auth_headers(domain, client_id, sd, Some(&overrides)),
    )
    .await?;

    if let Some(rscd) = check_rscd_from_headers(&response.headers) {
        return Err(rscd_service_error(rscd, &response.text()).into());
    }

    if response.status == 409 {
        // The 409 body carries the 2FA challenge metadata. When the account
        // has FIDO/WebAuthn security keys registered, Apple includes an
        // `fsaChallenge` object (and usually a `keyNames` array). CloudKit
        // rejects sessions minted through this flow with "no auth method
        // found" (issue #221), so bail before prompting for a 2FA code the
        // user can't complete headless.
        let challenge: super::responses::TwoFactorChallenge = response.json().unwrap_or_default();
        if challenge.requires_fido() {
            tracing::debug!(
                key_count = challenge.key_names.len(),
                "SRP complete returned 409 with FIDO/WebAuthn challenge; unsupported"
            );
            return Err(AuthError::FidoNotSupported {
                key_names: challenge.key_names,
            }
            .into());
        }
        tracing::debug!("SRP complete returned 409: two-factor authentication required");
        return Ok(());
    } else if response.status == 412 {
        tracing::debug!("SRP complete returned 412: attempting repair");
        // Session_data was already refreshed via extract_and_save on the
        // preceding complete response, so the headers here pick up any
        // rotated scnt/session_id. Route through `srp_post` so transient
        // 5xx/429 on the repair endpoint get the same retry policy as init
        // and complete.
        let repair_headers = get_auth_headers(domain, client_id, transport.session_data(), None)?;
        let repair_url = format!("{}/repair/complete", endpoints.auth);
        let mut repair_prebuilt = Some(repair_headers);
        let repair_response = srp_post(
            transport,
            "repair",
            &repair_url,
            "{}",
            &mut repair_prebuilt,
            |sd| get_auth_headers(domain, client_id, sd, None),
        )
        .await?;
        if !repair_response.is_success() {
            return Err(AuthError::ApiError {
                code: 412,
                message: format!("Repair failed: {}", repair_response.text()),
            }
            .into());
        }
    } else if response.is_server_error() {
        let status = response.status;
        let body = response.text();
        let detail = if body.contains('<') {
            String::new()
        } else {
            format!(": {body}")
        };
        return Err(AuthError::ApiError {
            code: status,
            message: format!(
                "Apple returned HTTP {status}{detail} — this is usually a temporary \
                 Apple server issue, try again in a few minutes"
            ),
        }
        .into());
    } else if response.status == 400 {
        // 400 from SRP complete means Apple rejected the payload shape, not
        // the password. Treat as a kei bug or protocol change; never blame
        // credentials.
        let body = response.text();
        let detail = if body.contains('<') {
            String::new()
        } else {
            format!(": {body}")
        };
        return Err(AuthError::ApiError {
            code: 400,
            message: format!(
                "Apple rejected the SRP payload as malformed (HTTP 400){detail}. \
                 This usually indicates a kei bug or an Apple auth protocol change."
            ),
        }
        .into());
    } else if response.status == 401 {
        // 401 at /signin/complete is the only status that reliably means the
        // password is wrong: Apple's SRP M1 verification rejected the proof.
        let body = response.text();
        let detail = if body.contains('<') {
            String::new()
        } else {
            format!(": {body}")
        };
        return Err(
            AuthError::FailedLogin(format!("Invalid email/password combination{detail}")).into(),
        );
    } else if response.status == 429 {
        return Err(AuthError::ApiError {
            code: 429,
            message:
                "Apple is rate limiting authentication (HTTP 429). Wait a few minutes and retry."
                    .into(),
        }
        .into());
    } else if response.is_client_error() {
        // Any other 4xx — Apple has historically returned 403 for rate
        // limits and rotated routing cookies, so surface the raw status
        // rather than attributing it to bad credentials.
        let status = response.status;
        let body = response.text();
        let detail = if body.contains('<') {
            String::new()
        } else {
            format!(": {body}")
        };
        return Err(AuthError::ApiError {
            code: status,
            message: format!("SRP complete rejected by Apple (HTTP {status}){detail}"),
        }
        .into());
    }

    Ok(())
}

/// POST to an Apple SRP endpoint, retrying on transient 429/5xx responses.
///
/// The first call uses `prebuilt_headers` so the caller's carefully-ordered
/// header map (with Origin/Referer overrides) is preserved. Retries call
/// `rebuild_headers` against the latest `session_data`, since Apple rotates
/// `scnt`/`session_id` on many responses and the retry would otherwise carry
/// stale values.
///
/// After the retry budget is exhausted, the last response is returned as
/// `Ok`; the caller's status-match sees the lingering 429/5xx and produces
/// the user-facing `AuthError`.
async fn srp_post<F>(
    transport: &mut impl SrpTransport,
    step: &'static str,
    url: &str,
    body: &str,
    prebuilt_headers: &mut Option<HeaderMap>,
    rebuild_headers: F,
) -> Result<SrpResponse>
where
    F: Fn(&HashMap<String, String>) -> Result<HeaderMap>,
{
    let max_delay = Duration::from_secs(AUTH_RETRY_CONFIG.max_delay_secs);
    let total_attempts = AUTH_RETRY_CONFIG.max_retries.saturating_add(1);
    let mut last_transient: Option<SrpResponse> = None;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..total_attempts {
        let headers = if let Some(h) = prebuilt_headers.take() {
            h
        } else {
            rebuild_headers(transport.session_data())?
        };
        match transport.post(url, Some(body), Some(headers)).await {
            Ok(resp) => {
                let status = resp.status;
                let is_transient = status == 429 || resp.is_server_error();
                if !is_transient || attempt + 1 >= total_attempts {
                    return Ok(resp);
                }
                let delay = parse_retry_after_header(&resp.headers, max_delay)
                    .unwrap_or_else(|| AUTH_RETRY_CONFIG.delay_for_retry(attempt));
                tracing::warn!(
                    attempt = attempt + 1,
                    total_attempts,
                    status,
                    retry_delay_secs = delay.as_secs(),
                    "SRP {step}: transient HTTP failure, retrying"
                );
                last_transient = Some(resp);
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                let is_last = attempt + 1 >= total_attempts;
                let is_network_error = e
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|r| r.status().is_none());
                if is_last || !is_network_error {
                    return Err(e);
                }
                let delay = AUTH_RETRY_CONFIG.delay_for_retry(attempt);
                tracing::warn!(
                    attempt = attempt + 1,
                    total_attempts,
                    error = %e,
                    retry_delay_secs = delay.as_secs(),
                    "SRP {step}: network error, retrying"
                );
                last_err = Some(e);
                tokio::time::sleep(delay).await;
            }
        }
    }
    if let Some(resp) = last_transient {
        return Ok(resp);
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("SRP {step}: retry loop exhausted")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_apple_password_s2k() {
        let key = derive_apple_password("testpass", "s2k", b"salt1234", 1000);
        assert_eq!(key.len(), 32);
        // Deterministic: same inputs produce same output
        let key2 = derive_apple_password("testpass", "s2k", b"salt1234", 1000);
        assert_eq!(key, key2);
    }

    #[test]
    fn test_derive_apple_password_s2k_fo() {
        let key = derive_apple_password("testpass", "s2k_fo", b"salt1234", 1000);
        assert_eq!(key.len(), 32);
        // s2k_fo uses hex encoding of hash, so result differs from s2k
        let key_s2k = derive_apple_password("testpass", "s2k", b"salt1234", 1000);
        assert_ne!(key, key_s2k);
    }

    #[test]
    fn test_compute_x_deterministic() {
        let salt = b"test_salt";
        let password_key = b"test_password_key";
        let x1 = compute_x(salt, password_key);
        let x2 = compute_x(salt, password_key);
        assert_eq!(x1, x2);
        assert!(x1 > BigUint::ZERO);
    }

    #[test]
    fn test_compute_k_deterministic() {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let g = BigUint::from(G_VAL);
        let k = compute_k(&n, &g);
        assert!(k > BigUint::ZERO);
        assert!(k < n);
    }

    #[test]
    fn test_compute_u_deterministic() {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let a = BigUint::from(12345u64);
        let b = BigUint::from(67890u64);
        let u1 = compute_u(&a, &b, &n);
        let u2 = compute_u(&a, &b, &n);
        assert_eq!(u1, u2);
    }

    #[test]
    fn test_compute_m1_and_m2_deterministic() {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let g = BigUint::from(G_VAL);
        let a_pub = BigUint::from(100u64);
        let b_pub = BigUint::from(200u64);
        let key = vec![0u8; 32];
        let m1 = compute_m1(&n, &g, b"user@test.com", b"salt", &a_pub, &b_pub, &key);
        assert_eq!(m1.len(), 32); // SHA-256 output
        let m2 = compute_m2(&a_pub, &m1, &key);
        assert_eq!(m2.len(), 32);
    }

    #[test]
    fn test_get_auth_headers_com_domain() {
        let session_data = HashMap::new();
        let headers = get_auth_headers("com", "client123", &session_data, None).unwrap();
        assert_eq!(
            headers.get("X-Apple-OAuth-Redirect-URI").unwrap(),
            "https://www.icloud.com"
        );
    }

    #[test]
    fn test_get_auth_headers_cn_domain() {
        let session_data = HashMap::new();
        let headers = get_auth_headers("cn", "client123", &session_data, None).unwrap();
        assert_eq!(
            headers.get("X-Apple-OAuth-Redirect-URI").unwrap(),
            "https://www.icloud.com.cn"
        );
    }

    #[test]
    fn test_get_auth_headers_with_session_data() {
        let mut session_data = HashMap::new();
        session_data.insert("scnt".to_string(), "test_scnt".to_string());
        session_data.insert("session_id".to_string(), "test_session".to_string());
        let headers = get_auth_headers("com", "client123", &session_data, None).unwrap();
        assert_eq!(headers.get("scnt").unwrap(), "test_scnt");
        assert_eq!(
            headers.get("X-Apple-ID-Session-Id").unwrap(),
            "test_session"
        );
    }

    // --- SRP orchestration tests ---

    fn srp_init_body(b_b64: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "salt": BASE64.encode(b"salt1234"),
            "b": b_b64,
            "c": "challenge_token",
            "iteration": 1,
            "protocol": "s2k"
        }))
        .unwrap()
    }

    fn response(status: u16, body: Vec<u8>) -> SrpResponse {
        SrpResponse {
            status,
            body,
            headers: HeaderMap::new(),
        }
    }

    fn response_with_headers(status: u16, body: Vec<u8>, headers: HeaderMap) -> SrpResponse {
        SrpResponse {
            status,
            body,
            headers,
        }
    }

    /// A valid SRP init response with B = 2 (non-zero mod N).
    fn valid_init_response() -> SrpResponse {
        response(200, srp_init_body(&BASE64.encode([2u8])))
    }

    struct StubSrpTransport {
        responses: std::collections::VecDeque<SrpResponse>,
        session_data: HashMap<String, String>,
    }

    #[async_trait::async_trait]
    impl SrpTransport for StubSrpTransport {
        async fn post(
            &mut self,
            _url: &str,
            _body: Option<&str>,
            _headers: Option<HeaderMap>,
        ) -> Result<SrpResponse> {
            Ok(self
                .responses
                .pop_front()
                .expect("StubSrpTransport: no more responses"))
        }

        fn session_data(&self) -> &HashMap<String, String> {
            &self.session_data
        }
    }

    fn stub(responses: Vec<SrpResponse>) -> StubSrpTransport {
        StubSrpTransport {
            responses: responses.into(),
            session_data: HashMap::new(),
        }
    }

    async fn run_srp(responses: Vec<SrpResponse>) -> Result<()> {
        let mut t = stub(responses);
        let ep = Endpoints::for_domain("com").unwrap();
        authenticate_srp(&mut t, &ep, "u@test.com", "p", "c", "com").await
    }

    /// 401 at /signin/init is not a bad-password signal — Apple hasn't yet
    /// verified the M1 proof. Expect a typed `ApiError` so the caller doesn't
    /// tell the user their password is wrong for a transient auth-CDN issue.
    #[tokio::test]
    async fn srp_init_401_returns_api_error_not_failed_login() {
        let err = run_srp(vec![response(401, vec![])]).await.unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(
            matches!(auth_err, AuthError::ApiError { code: 401, .. }),
            "expected ApiError {{ code: 401 }}, got: {auth_err:?}"
        );
        assert!(
            !matches!(auth_err, AuthError::FailedLogin(_)),
            "401 on init must NOT be FailedLogin (would blame password)"
        );
    }

    /// srp_post retries transient 5xx until the budget is exhausted, then
    /// surfaces an ApiError carrying the final status.
    #[tokio::test(start_paused = true)]
    async fn srp_init_500_retries_then_returns_api_error() {
        let err = run_srp(vec![
            response(500, b"server error".to_vec()),
            response(500, b"server error".to_vec()),
            response(500, b"server error".to_vec()),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(matches!(auth_err, AuthError::ApiError { code: 500, .. }));
    }

    /// A 503 that recovers on retry should succeed, proving the retry budget
    /// is actually being used.
    #[tokio::test(start_paused = true)]
    async fn srp_init_503_retries_then_succeeds() {
        run_srp(vec![
            response(503, b"unavailable".to_vec()),
            valid_init_response(),
            response(200, vec![]),
        ])
        .await
        .unwrap();
    }

    /// `srp_post` must honor a `Retry-After` header on a transient 429
    /// response. With paused time, a finite header delay proves the parse
    /// path is wired up (a broken parse would fall through to the much
    /// larger exponential backoff, but both succeed here — the test still
    /// documents the contract and would surface as a compile-time regression
    /// if `parse_retry_after_header` were dropped from srp_post).
    #[tokio::test(start_paused = true)]
    async fn srp_init_429_with_retry_after_retries_then_succeeds() {
        let mut retry_after = HeaderMap::new();
        retry_after.insert("Retry-After", HeaderValue::from_static("1"));
        run_srp(vec![
            response_with_headers(429, b"too many".to_vec(), retry_after),
            valid_init_response(),
            response(200, vec![]),
        ])
        .await
        .unwrap();
    }

    /// Apple's `X-Apple-I-Rscd: 401/403` header means "session rejected" even
    /// when the HTTP status is 200. SRP must detect this so a hidden rejection
    /// surfaces as a ServiceError rather than being treated as a valid handshake.
    #[tokio::test]
    async fn srp_init_rscd_401_on_http_200_is_service_error() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Apple-I-Rscd", HeaderValue::from_static("401"));
        let err = run_srp(vec![response_with_headers(
            200,
            srp_init_body(&BASE64.encode([2u8])),
            headers,
        )])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        match auth_err {
            AuthError::ServiceError { code, .. } => assert_eq!(code, "rscd_401"),
            other => panic!("expected rscd ServiceError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn srp_complete_rscd_403_on_http_200_is_service_error() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Apple-I-Rscd", HeaderValue::from_static("403"));
        let err = run_srp(vec![
            valid_init_response(),
            response_with_headers(200, vec![], headers),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        match auth_err {
            AuthError::ServiceError { code, .. } => assert_eq!(code, "rscd_403"),
            other => panic!("expected rscd ServiceError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn srp_init_invalid_json_returns_parse_error() {
        let err = run_srp(vec![response(200, b"not json".to_vec())])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SRP init: expected JSON"));
    }

    #[tokio::test]
    async fn srp_b_mod_n_zero_returns_error() {
        let err = run_srp(vec![response(200, srp_init_body(&BASE64.encode([0u8])))])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("B mod N is zero"));
    }

    #[tokio::test]
    async fn srp_happy_path() {
        run_srp(vec![valid_init_response(), response(200, vec![])])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn srp_complete_409_signals_2fa_required() {
        run_srp(vec![valid_init_response(), response(409, vec![])])
            .await
            .unwrap();
    }

    /// 409 with an `fsaChallenge` field means the account has a FIDO/WebAuthn
    /// security key registered. kei can't complete that challenge, so the
    /// caller must see a typed `FidoNotSupported` error rather than
    /// proceeding to prompt for a 2FA code (which would hang or fail
    /// silently downstream at the CloudKit handshake).
    #[tokio::test]
    async fn srp_complete_409_with_fsa_challenge_returns_fido_not_supported() {
        let body = br#"{
            "fsaChallenge": {"challenge": "abc", "keyHandles": ["h1"], "rpId": "apple.com"},
            "keyNames": ["YubiKey 5C"],
            "authType": "hsa2"
        }"#;
        let err = run_srp(vec![valid_init_response(), response(409, body.to_vec())])
            .await
            .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        match auth_err {
            AuthError::FidoNotSupported { key_names } => {
                assert_eq!(key_names, &vec!["YubiKey 5C".to_string()]);
            }
            other => panic!("expected FidoNotSupported, got: {other:?}"),
        }
    }

    /// Defensive: Apple could send `keyNames` without `fsaChallenge` in a
    /// flow we haven't observed. Treat the presence of any named security
    /// keys as a FIDO signal.
    #[tokio::test]
    async fn srp_complete_409_with_only_key_names_returns_fido_not_supported() {
        let body = br#"{"keyNames": ["Passkey-Home"]}"#;
        let err = run_srp(vec![valid_init_response(), response(409, body.to_vec())])
            .await
            .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(
            matches!(auth_err, AuthError::FidoNotSupported { .. }),
            "expected FidoNotSupported, got: {auth_err:?}"
        );
    }

    /// A normal HSA2 challenge (push to trusted device, SMS to trusted
    /// phone) must continue through the existing 2FA flow — the FIDO
    /// detection must not false-positive on the usual 409 shape.
    #[tokio::test]
    async fn srp_complete_409_without_fido_fields_passes_through() {
        let body = br#"{
            "trustedDevices": [{"id": "d1"}],
            "trustedPhoneNumbers": [{"id": 1}],
            "authType": "hsa2",
            "securityCode": {"length": 6}
        }"#;
        run_srp(vec![valid_init_response(), response(409, body.to_vec())])
            .await
            .unwrap();
    }

    /// If Apple returns a 409 with an unparsable body (rare but possible on
    /// upstream errors or protocol drift), the default-derived
    /// `TwoFactorChallenge` reports no FIDO, and SRP falls through to the
    /// normal 2FA path rather than incorrectly bailing.
    #[tokio::test]
    async fn srp_complete_409_with_unparsable_body_falls_through_to_2fa() {
        run_srp(vec![
            valid_init_response(),
            response(409, b"not json".to_vec()),
        ])
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn srp_complete_412_repair_succeeds() {
        run_srp(vec![
            valid_init_response(),
            response(412, vec![]),
            response(200, vec![]),
        ])
        .await
        .unwrap();
    }

    /// Persistent 5xx on /repair/complete exhausts the retry budget and
    /// surfaces as `ApiError { code: 412 }` with the repair-failed message.
    /// Three responses needed: init (200), complete (412), then the repair
    /// call consumes AUTH_RETRY_CONFIG.max_retries+1 retries.
    #[tokio::test(start_paused = true)]
    async fn srp_complete_412_repair_fails() {
        let err = run_srp(vec![
            valid_init_response(),
            response(412, vec![]),
            response(500, b"repair broken".to_vec()),
            response(500, b"repair broken".to_vec()),
            response(500, b"repair broken".to_vec()),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(matches!(auth_err, AuthError::ApiError { code: 412, .. }));
    }

    /// A transient 5xx on /repair/complete is retried by srp_post; on a
    /// later 2xx the overall SRP flow succeeds.
    #[tokio::test(start_paused = true)]
    async fn srp_complete_412_repair_retries_transient_5xx() {
        run_srp(vec![
            valid_init_response(),
            response(412, vec![]),
            response(503, b"unavailable".to_vec()),
            response(200, vec![]),
        ])
        .await
        .unwrap();
    }

    /// 401 at /signin/complete IS a password rejection (SRP's M1 verification
    /// failed). This is the one branch that may legitimately say "wrong password".
    #[tokio::test]
    async fn srp_complete_401_is_failed_login() {
        let err = run_srp(vec![
            valid_init_response(),
            response(401, b"wrong".to_vec()),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(
            matches!(auth_err, AuthError::FailedLogin(_)),
            "401 on complete should be FailedLogin, got: {auth_err:?}"
        );
    }

    /// 403 at /signin/complete should NOT be mis-attributed to bad password.
    /// Apple returns 403 for rate limits, rotated routing cookies, and more.
    #[tokio::test]
    async fn srp_complete_403_is_api_error_not_failed_login() {
        let err = run_srp(vec![
            valid_init_response(),
            response(403, b"forbidden".to_vec()),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(
            matches!(auth_err, AuthError::ApiError { code: 403, .. }),
            "403 on complete must be ApiError, not FailedLogin, got: {auth_err:?}"
        );
    }

    /// 429 at /signin/complete must be reported as rate-limited, not as a
    /// password failure. Retries inside srp_post handle transient cases; this
    /// path covers exhaustion.
    #[tokio::test(start_paused = true)]
    async fn srp_complete_429_is_rate_limited_api_error() {
        let err = run_srp(vec![
            valid_init_response(),
            response(429, b"too many".to_vec()),
            response(429, b"too many".to_vec()),
            response(429, b"too many".to_vec()),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(
            matches!(auth_err, AuthError::ApiError { code: 429, .. }),
            "429 on complete must be ApiError(429), got: {auth_err:?}"
        );
    }

    /// 400 at /signin/complete signals a malformed payload — kei bug or
    /// Apple protocol change. Never a wrong password.
    #[tokio::test]
    async fn srp_complete_400_is_api_error() {
        let err = run_srp(vec![
            valid_init_response(),
            response(400, b"bad request".to_vec()),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        match auth_err {
            AuthError::ApiError { code: 400, message } => {
                assert!(
                    message.to_lowercase().contains("malformed")
                        || message.to_lowercase().contains("400"),
                    "expected explanatory 400 message, got: {message}"
                );
            }
            other => panic!("expected ApiError(400), got: {other:?}"),
        }
    }

    /// Persistent 5xx after retry exhaustion is surfaced as ApiError (not
    /// FailedLogin, which is the bug this PR fixes).
    #[tokio::test(start_paused = true)]
    async fn srp_complete_server_error_returns_api_error() {
        let err = run_srp(vec![
            valid_init_response(),
            response(502, b"bad gateway".to_vec()),
            response(502, b"bad gateway".to_vec()),
            response(502, b"bad gateway".to_vec()),
        ])
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().unwrap();
        assert!(
            matches!(auth_err, AuthError::ApiError { code: 502, .. }),
            "5xx after retries must be ApiError, got: {auth_err:?}"
        );
    }

    /// Transient 503 on complete that recovers within the retry budget
    /// should succeed rather than surfacing an error.
    #[tokio::test(start_paused = true)]
    async fn srp_complete_503_retries_then_succeeds() {
        run_srp(vec![
            valid_init_response(),
            response(503, b"unavailable".to_vec()),
            response(200, vec![]),
        ])
        .await
        .unwrap();
    }

    // ────────────────────────────────────────────────────────────────
    // wiremock-based tests: exercise the FIDO detection path through a
    // real HTTP client and SRP math, not the StubSrpTransport. Guards
    // against regressions where the 409 body parsing works in isolation
    // but the reqwest buffering, status-code branch, or JSON decode
    // path drops the signal before `authenticate_srp` sees it.
    // ────────────────────────────────────────────────────────────────

    use crate::auth::session::Session;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn wm_session(server: &MockServer) -> (TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path(), "test@example.com", &server.uri(), Some(5))
            .await
            .unwrap();
        (dir, session)
    }

    /// Minimal SRP init body that produces tractable math in tests:
    /// `B = 2`, `iteration = 1`. Apple's real responses have iteration
    /// counts in the tens of thousands; 1 is fine because the point of
    /// this test isn't to exercise PBKDF2, it's to drive the request
    /// through the HTTP layer to the 409 branch.
    fn wm_srp_init_body() -> String {
        serde_json::json!({
            "salt": BASE64.encode(b"salt1234"),
            "b": BASE64.encode([2u8]),
            "c": "challenge-token",
            "iteration": 1,
            "protocol": "s2k"
        })
        .to_string()
    }

    /// End-to-end: real SRP handshake against a wiremock server that
    /// returns 409 with an `fsaChallenge`. The test proves the FIDO
    /// signal survives the full HTTP round-trip (reqwest buffering,
    /// status check, JSON parse) and surfaces as the typed
    /// `FidoNotSupported` error with the key names Apple disclosed.
    #[tokio::test]
    async fn srp_wiremock_fsa_challenge_returns_fido_not_supported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/init"))
            .respond_with(ResponseTemplate::new(200).set_body_string(wm_srp_init_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/complete"))
            .respond_with(ResponseTemplate::new(409).set_body_string(
                r#"{
                    "fsaChallenge": {"challenge": "abc", "keyHandles": ["h1"], "rpId": "apple.com"},
                    "keyNames": ["YubiKey 5C", "Passkey-Home"],
                    "authType": "hsa2",
                    "trustedDevices": []
                }"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let (_dir, mut session) = wm_session(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let err = authenticate_srp(
            &mut session,
            &endpoints,
            "test@example.com",
            "hunter2",
            "client-id",
            "com",
        )
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().expect("typed AuthError");
        match auth_err {
            AuthError::FidoNotSupported { key_names } => {
                assert_eq!(
                    key_names,
                    &vec!["YubiKey 5C".to_string(), "Passkey-Home".to_string()],
                    "key_names must round-trip verbatim so the error message can name the keys"
                );
            }
            other => panic!("expected FidoNotSupported, got: {other:?}"),
        }
    }

    /// End-to-end control: an ordinary device-push 2FA 409 (no security
    /// keys) must NOT trigger FIDO detection. `authenticate_srp` returns
    /// `Ok(())` so the caller can prompt for the code. Guards against a
    /// future refactor that over-broadens the detection check.
    #[tokio::test]
    async fn srp_wiremock_ordinary_2fa_passes_through() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/init"))
            .respond_with(ResponseTemplate::new(200).set_body_string(wm_srp_init_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/complete"))
            .respond_with(ResponseTemplate::new(409).set_body_string(
                r#"{
                    "trustedDevices": [{"id": "d1"}],
                    "trustedPhoneNumbers": [{"id": 1, "numberWithDialCode": "+1 •••-•••-1234"}],
                    "authType": "hsa2",
                    "securityCode": {"length": 6}
                }"#,
            ))
            .mount(&server)
            .await;

        let (_dir, mut session) = wm_session(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        authenticate_srp(
            &mut session,
            &endpoints,
            "test@example.com",
            "hunter2",
            "client-id",
            "com",
        )
        .await
        .expect("ordinary 2FA 409 must not be mis-classified as FIDO");
    }

    /// A 409 with `keyNames` but no `fsaChallenge` must still bail as
    /// FIDO. Defensive against Apple flow variants we haven't directly
    /// observed.
    #[tokio::test]
    async fn srp_wiremock_key_names_only_returns_fido_not_supported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/init"))
            .respond_with(ResponseTemplate::new(200).set_body_string(wm_srp_init_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/complete"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_string(r#"{"keyNames": ["Solo V2"], "authType": "hsa2"}"#),
            )
            .mount(&server)
            .await;

        let (_dir, mut session) = wm_session(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        let err = authenticate_srp(
            &mut session,
            &endpoints,
            "test@example.com",
            "hunter2",
            "client-id",
            "com",
        )
        .await
        .unwrap_err();
        let auth_err = err.downcast_ref::<AuthError>().expect("typed AuthError");
        assert!(
            matches!(auth_err, AuthError::FidoNotSupported { .. }),
            "keyNames alone must still trigger FIDO bail, got: {auth_err:?}"
        );
    }

    /// A 409 with a malformed body must fall through to the normal 2FA
    /// path (returning `Ok(())`), not spuriously bail as FIDO. Guards
    /// against a future parser change that would treat a parse error as
    /// "FIDO present".
    #[tokio::test]
    async fn srp_wiremock_unparsable_409_falls_through_to_2fa() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/init"))
            .respond_with(ResponseTemplate::new(200).set_body_string(wm_srp_init_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/appleauth/auth/signin/complete"))
            .respond_with(ResponseTemplate::new(409).set_body_string("<html>oops</html>"))
            .mount(&server)
            .await;

        let (_dir, mut session) = wm_session(&server).await;
        let endpoints = Endpoints::for_test_base(&server.uri());
        authenticate_srp(
            &mut session,
            &endpoints,
            "test@example.com",
            "hunter2",
            "client-id",
            "com",
        )
        .await
        .expect("unparsable 409 must fall through to the 2FA path, not bail as FIDO");
    }
}
