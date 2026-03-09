use serde::Deserialize;
use serde_json::Value;

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

/// Response from `/accountLogin` and `/validate` — carries the account's
/// service URLs, 2FA state, and directory service info.
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
