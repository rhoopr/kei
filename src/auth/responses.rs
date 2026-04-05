use serde::Deserialize;
use serde_json::Value;

use super::error::AuthError;

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

/// An error entry from Apple's `service_errors` array.
/// Apple auth APIs sometimes return HTTP 200 with error details in the body
/// instead of using HTTP status codes.
#[derive(Debug, Deserialize)]
pub struct AppleServiceError {
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
#[derive(Debug, Deserialize)]
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
}

impl AccountLoginResponse {
    /// Check for Apple's body-level error indicators.
    ///
    /// Returns `Ok(())` if no errors, or `Err(AuthError::ServiceError)` if
    /// `hasError` is true or `service_errors` is non-empty.
    pub fn check_errors(&self) -> Result<(), AuthError> {
        if let Some(err) = self.service_errors.first() {
            return Err(AuthError::ServiceError {
                code: err.code.clone(),
                message: if err.message.is_empty() {
                    err.title.clone().unwrap_or_else(|| "unknown".to_string())
                } else {
                    err.message.clone()
                },
            });
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DsInfo {
    #[serde(default)]
    pub hsa_version: i64,
    #[serde(default)]
    pub dsid: Option<String>,
    #[serde(default)]
    pub has_i_cloud_qualifying_device: bool,
}

#[derive(Debug, Deserialize)]
pub struct Webservices {
    #[serde(default)]
    pub ckdatabasews: Option<WebserviceEndpoint>,
}

#[derive(Debug, Deserialize)]
pub struct WebserviceEndpoint {
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
