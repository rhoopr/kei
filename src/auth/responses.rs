use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::AuthError;

/// Default grace period for session validation cache (10 minutes).
/// Within this window, `authenticate` skips the Apple `/validate` call
/// and reuses cached account data from the previous validation.
pub(crate) const VALIDATION_CACHE_GRACE_SECS: i64 = 600;

/// Cached result from a successful `/validate` or `/accountLogin` call.
/// Stored alongside the session file as `{username}.cache` so that
/// rapid successive kei invocations don't hammer Apple's auth endpoints.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ValidationCache {
    /// Unix timestamp when the session was last validated.
    pub validated_at: i64,
    /// The account data returned by Apple.
    pub account_data: AccountLoginResponse,
}

/// Server's half of the SRP handshake — contains the salt, public ephemeral B,
/// iteration count, and protocol variant needed to compute the shared secret.
#[derive(Debug, Deserialize)]
pub struct SrpInitResponse {
    pub salt: String,
    pub b: String,
    /// Opaque challenge token echoed back in `/signin/complete`.
    pub c: Value,
    pub iteration: u64,
    pub protocol: String,
}

/// Subset of Apple's `/signin/complete` 409 body used to detect FIDO/WebAuthn
/// security-key requirements. When `fsa_challenge` is present or `key_names`
/// is non-empty, the account requires a security-key tap that kei can't
/// perform headless. Sessions minted through this flow get rejected by
/// CloudKit with "no auth method found" (issue #221), so we bail early.
///
/// Other 2FA-challenge fields (`trustedDevices`, `trustedPhoneNumbers`,
/// `securityCode`) are ignored here — they're handled further down the
/// existing 2FA prompt path.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TwoFactorChallenge {
    /// Present iff Apple is asking for a FIDO/WebAuthn assertion.
    /// The object's internals (challenge, keyHandles, rpId) are not
    /// inspected — presence alone is the signal.
    #[serde(default)]
    pub fsa_challenge: Option<Value>,
    /// Human-readable names of registered security keys, e.g.
    /// `["YubiKey 5C"]`. Surfaced verbatim so the user can identify
    /// which keys to remove.
    #[serde(default)]
    pub key_names: Vec<String>,
}

impl TwoFactorChallenge {
    /// True if Apple's 2FA challenge includes a FIDO/WebAuthn assertion.
    pub(crate) fn requires_fido(&self) -> bool {
        self.fsa_challenge.is_some() || !self.key_names.is_empty()
    }
}

/// An error entry from Apple's `service_errors` array.
/// Apple auth APIs sometimes return HTTP 200 with error details in the body
/// instead of using HTTP status codes.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct AppleServiceError {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// Response from `/accountLogin` and `/validate` — carries the account's
/// service URLs, 2FA state, and directory service info.
///
/// Apple sometimes returns HTTP 200 with `hasError: true` and a
/// `service_errors` array instead of a proper HTTP error status.
/// Call [`check_errors()`](Self::check_errors) after deserializing to detect these.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLoginResponse {
    #[serde(default)]
    pub ds_info: Option<DsInfo>,
    #[serde(default)]
    pub webservices: Option<Webservices>,
    #[serde(default)]
    pub hsa_challenge_required: bool,
    #[serde(default)]
    pub hsa_trusted_browser: bool,
    #[serde(default)]
    pub domain_to_use: Option<String>,
    #[serde(default, alias = "has_error")]
    pub has_error: bool,
    #[serde(default, alias = "service_errors")]
    pub service_errors: Vec<AppleServiceError>,
    /// Whether Advanced Data Protection (iCloud end-to-end encryption) is
    /// active on the account.  Apple names this field `iCDPEnabled` in the
    /// `/accountLogin` and `/validate` responses.
    #[serde(default, alias = "iCDPEnabled")]
    pub i_cdp_enabled: bool,
}

impl AccountLoginResponse {
    /// Check for Apple's body-level error indicators.
    ///
    /// Returns `Ok(())` if no errors, or `Err(AuthError::ServiceError)` if
    /// `hasError` is true or `service_errors` is non-empty.
    pub fn check_errors(&self) -> Result<(), AuthError> {
        if let Some(err) = self.service_errors.first() {
            let raw_message = if err.message.is_empty() {
                err.title.as_deref().unwrap_or("unknown")
            } else {
                &err.message
            };
            return Err(AuthError::service_error(&err.code, raw_message));
        }
        if self.has_error {
            return Err(AuthError::ServiceError {
                code: "unknown".to_string(),
                message: "Apple reported an error but provided no details".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DsInfo {
    #[serde(default)]
    pub hsa_version: i64,
    #[serde(default)]
    pub dsid: Option<String>,
    #[serde(default)]
    pub has_i_cloud_qualifying_device: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Webservices {
    #[serde(default)]
    pub ckdatabasews: Option<WebserviceEndpoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WebserviceEndpoint {
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_factor_challenge_detects_fsa_challenge() {
        let json = r#"{
            "trustedDevices": [],
            "authType": "hsa2",
            "fsaChallenge": {
                "challenge": "abc123",
                "keyHandles": ["handle-1"],
                "rpId": "apple.com"
            },
            "keyNames": ["YubiKey 5C"]
        }"#;
        let parsed: TwoFactorChallenge = serde_json::from_str(json).unwrap();
        assert!(parsed.requires_fido());
        assert_eq!(parsed.key_names, vec!["YubiKey 5C".to_string()]);
    }

    #[test]
    fn two_factor_challenge_detects_key_names_without_fsa_challenge() {
        // Defensive: Apple could send keyNames without fsaChallenge in some
        // flow we haven't seen; still treat as FIDO.
        let json = r#"{"keyNames": ["YubiKey 5C", "Passkey-Home"]}"#;
        let parsed: TwoFactorChallenge = serde_json::from_str(json).unwrap();
        assert!(parsed.requires_fido());
        assert_eq!(parsed.key_names.len(), 2);
    }

    #[test]
    fn two_factor_challenge_ignores_device_only_2fa() {
        // A normal HSA2 challenge (no security keys) must NOT match.
        let json = r#"{
            "trustedDevices": [{"id": "d1"}],
            "trustedPhoneNumbers": [{"id": 1, "numberWithDialCode": "+1 •••-•••-1234"}],
            "authType": "hsa2",
            "securityCode": {"length": 6}
        }"#;
        let parsed: TwoFactorChallenge = serde_json::from_str(json).unwrap();
        assert!(!parsed.requires_fido());
        assert!(parsed.key_names.is_empty());
    }

    #[test]
    fn two_factor_challenge_defaults_on_missing_fields() {
        // Minimal body must deserialize cleanly and report no FIDO.
        let parsed: TwoFactorChallenge = serde_json::from_str("{}").unwrap();
        assert!(!parsed.requires_fido());
    }

    #[test]
    fn two_factor_challenge_handles_empty_body_via_default() {
        // The SRP handler calls `.unwrap_or_default()` when the 409 body
        // isn't valid JSON. The default must report no FIDO so a transient
        // parse miss doesn't incorrectly bail a legitimate device-push
        // 2FA flow.
        let parsed = TwoFactorChallenge::default();
        assert!(!parsed.requires_fido());
        assert!(parsed.key_names.is_empty());
    }

    #[test]
    fn test_srp_init_response_deserialize() {
        let json = r#"{
            "salt": "abc123",
            "b": "def456",
            "c": {"key": "value"},
            "iteration": 20000,
            "protocol": "s2k_fo"
        }"#;
        let resp: SrpInitResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.salt, "abc123");
        assert_eq!(resp.b, "def456");
        assert_eq!(resp.iteration, 20000);
        assert_eq!(resp.protocol, "s2k_fo");
    }

    #[test]
    fn test_account_login_response_full() {
        let json = r#"{
            "dsInfo": {
                "hsaVersion": 2,
                "dsid": "12345",
                "hasICloudQualifyingDevice": true
            },
            "webservices": {
                "ckdatabasews": {
                    "url": "https://p123-ckdatabasews.icloud.com"
                }
            },
            "hsaChallengeRequired": true,
            "hsaTrustedBrowser": false,
            "domainToUse": null
        }"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        let ds = resp.ds_info.unwrap();
        assert_eq!(ds.hsa_version, 2);
        assert_eq!(ds.dsid.unwrap(), "12345");
        assert!(ds.has_i_cloud_qualifying_device);
        assert!(resp.hsa_challenge_required);
        assert!(!resp.hsa_trusted_browser);
        let ws = resp.webservices.unwrap();
        assert_eq!(
            ws.ckdatabasews.unwrap().url,
            "https://p123-ckdatabasews.icloud.com"
        );
    }

    #[test]
    fn test_account_login_response_minimal() {
        let json = r#"{}"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ds_info.is_none());
        assert!(resp.webservices.is_none());
        assert!(!resp.hsa_challenge_required);
    }

    #[test]
    fn test_account_login_response_extra_fields() {
        let json = r#"{"unknownField": 42, "hsaTrustedBrowser": true}"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert!(resp.hsa_trusted_browser);
    }

    #[test]
    fn test_account_login_response_has_error_camel_case() {
        let json = r#"{"hasError": true}"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert!(resp.has_error);
        let err = resp.check_errors().unwrap_err();
        assert!(err.to_string().contains("Apple reported an error"));
    }

    #[test]
    fn test_account_login_response_has_error_snake_case() {
        let json = r#"{"has_error": true}"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert!(resp.has_error);
        assert!(resp.check_errors().is_err());
    }

    #[test]
    fn test_account_login_response_service_errors() {
        let json = r#"{
            "hasError": true,
            "service_errors": [
                {"code": "AUTH-401", "message": "Authentication required", "title": "Error"}
            ]
        }"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.service_errors.len(), 1);
        assert_eq!(resp.service_errors[0].code, "AUTH-401");
        let err = resp.check_errors().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("AUTH-401"));
        assert!(msg.contains("Authentication required"));
    }

    #[test]
    fn test_account_login_response_service_errors_camel_case() {
        let json = r#"{
            "hasError": true,
            "serviceErrors": [
                {"code": "ERR-500", "message": "Internal error"}
            ]
        }"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.service_errors.len(), 1);
        assert!(resp.check_errors().is_err());
    }

    #[test]
    fn test_account_login_response_service_error_title_fallback() {
        let json = r#"{
            "service_errors": [{"code": "ERR", "message": "", "title": "Something went wrong"}]
        }"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        let err = resp.check_errors().unwrap_err();
        assert!(err.to_string().contains("Something went wrong"));
    }

    #[test]
    fn test_account_login_response_no_errors_passes() {
        let json = r#"{"hsaTrustedBrowser": true}"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert!(resp.check_errors().is_ok());
    }

    #[test]
    fn test_check_errors_zone_not_found_enriched() {
        let json = r#"{
            "service_errors": [{"code": "ZONE_NOT_FOUND", "message": "Zone not found"}]
        }"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        let err = resp.check_errors().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("icloud.com"),
            "should mention icloud.com: {msg}"
        );
    }

    #[test]
    fn test_check_errors_authentication_failed_enriched() {
        let json = r#"{
            "service_errors": [{"code": "AUTHENTICATION_FAILED", "message": "Auth failed"}]
        }"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        let err = resp.check_errors().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("set up"), "should suggest setup: {msg}");
    }

    #[test]
    fn test_check_errors_access_denied_enriched() {
        let json = r#"{
            "service_errors": [{"code": "ACCESS_DENIED", "message": "Access denied"}]
        }"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        let err = resp.check_errors().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("wait a few minutes"),
            "should suggest waiting: {msg}"
        );
    }

    #[test]
    fn test_account_login_response_domain_to_use() {
        let json = r#"{"domainToUse": "icloud.com.cn"}"#;
        let resp: AccountLoginResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.domain_to_use.as_deref(), Some("icloud.com.cn"));
    }

    #[test]
    fn test_ds_info_defaults() {
        let json = r#"{}"#;
        let ds: DsInfo = serde_json::from_str(json).unwrap();
        assert_eq!(ds.hsa_version, 0);
        assert!(ds.dsid.is_none());
        assert!(!ds.has_i_cloud_qualifying_device);
    }

    #[test]
    fn test_webservices_no_ckdatabasews() {
        let json = r#"{}"#;
        let ws: Webservices = serde_json::from_str(json).unwrap();
        assert!(ws.ckdatabasews.is_none());
    }

    #[test]
    fn test_webservice_endpoint() {
        let json = r#"{"url": "https://p99-ckdatabasews.icloud.com:443"}"#;
        let ep: WebserviceEndpoint = serde_json::from_str(json).unwrap();
        assert_eq!(ep.url, "https://p99-ckdatabasews.icloud.com:443");
    }
}
