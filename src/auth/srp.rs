use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use num_bigint::BigUint;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};

use std::collections::HashMap;

use super::endpoints::Endpoints;
use super::session::Session;
use crate::auth::error::AuthError;

/// Apple's public OAuth widget key — embedded in icloud.com's JavaScript.
const APPLE_WIDGET_KEY: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";

/// RFC 5054 2048-bit SRP group prime (same as srp::groups::G_2048).
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
    };

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
/// M1 always uses the real username (apple_id).
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
    for (i, (a, b)) in h_n.iter().zip(h_g.iter()).enumerate() {
        h_xor[i] = a ^ b;
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
    overrides: Option<HashMap<String, String>>,
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
        for (key, val) in ovr {
            if let Ok(v) = HeaderValue::from_str(&val) {
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
    session: &mut Session,
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

    let build_overrides = || {
        let mut ovr = HashMap::new();
        ovr.insert("Origin".to_string(), endpoints.auth_root.to_string());
        ovr.insert("Referer".to_string(), format!("{}/", endpoints.auth_root));
        ovr
    };

    let init_headers = get_auth_headers(
        domain,
        client_id,
        &session.session_data,
        Some(build_overrides()),
    )?;

    tracing::debug!("Initiating SRP authentication for {}", apple_id);

    let init_url = format!("{}/signin/init", endpoints.auth);
    let response = session
        .post(&init_url, Some(init_body.to_string()), Some(init_headers))
        .await?;

    let status = response.status();
    if status.as_u16() == 401 {
        return Err(AuthError::FailedLogin("Failed to initiate SRP authentication".into()).into());
    }
    if !status.is_success() && status.as_u16() != 409 {
        let text = response.text().await.unwrap_or_default();
        return Err(AuthError::ApiError {
            code: status.as_u16(),
            message: text,
        }
        .into());
    }

    let body: super::responses::SrpInitResponse = response
        .json()
        .await
        .context("Failed to parse SRP init response as JSON")?;

    let iterations = u32::try_from(body.iteration).context("SRP iteration count exceeds u32")?;

    let salt = BASE64
        .decode(&body.salt)
        .context("Failed to decode SRP salt")?;
    let b_pub_bytes = BASE64
        .decode(&body.b)
        .context("Failed to decode SRP public key")?;
    let b_pub = BigUint::from_bytes_be(&b_pub_bytes);

    let password_key = derive_apple_password(password, &body.protocol, &salt, iterations);

    tracing::debug!(
        "SRP protocol: {}, iterations: {}",
        body.protocol,
        iterations
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

    let trust_tokens: Vec<String> = session
        .session_data
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
    let complete_headers = get_auth_headers(
        domain,
        client_id,
        &session.session_data,
        Some(build_overrides()),
    )?;
    let complete_url = format!(
        "{}/signin/complete?isRememberMeEnabled=true",
        endpoints.auth
    );
    let response = session
        .post(
            &complete_url,
            Some(complete_body.to_string()),
            Some(complete_headers),
        )
        .await?;

    let status = response.status();
    if status.as_u16() == 409 {
        // 409 is Apple's signal that credentials are valid but 2FA is needed
        tracing::debug!("SRP complete returned 409: two-factor authentication required");
        return Ok(());
    } else if status.as_u16() == 412 {
        tracing::debug!("SRP complete returned 412: attempting repair");
        let repair_headers = get_auth_headers(domain, client_id, &session.session_data, None)?;
        let repair_url = format!("{}/repair/complete", endpoints.auth);
        let repair_response = session
            .post(&repair_url, Some("{}".to_string()), Some(repair_headers))
            .await?;
        if !repair_response.status().is_success() {
            let text = repair_response.text().await.unwrap_or_default();
            return Err(AuthError::ApiError {
                code: 412,
                message: format!("Repair failed: {}", text),
            }
            .into());
        }
    } else if status.is_client_error() || status.is_server_error() {
        let text = response.text().await.unwrap_or_default();
        return Err(AuthError::FailedLogin(format!(
            "Invalid email/password combination: {}",
            text
        ))
        .into());
    }

    Ok(())
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
}
