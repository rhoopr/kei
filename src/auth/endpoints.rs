/// URL endpoint constants for Apple iCloud authentication services.
/// Supports both "com" (international) and "cn" (China) domains.

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub auth_root: &'static str,
    pub auth: &'static str,
    pub home: &'static str,
    pub setup: &'static str,
}

impl Endpoints {
    /// Returns the correct endpoints for the given domain.
    ///
    /// Supported domains: "com" (international), "cn" (China mainland).
    pub fn for_domain(domain: &str) -> anyhow::Result<Self> {
        match domain {
            "com" => Ok(Self {
                auth_root: "https://idmsa.apple.com",
                auth: "https://idmsa.apple.com/appleauth/auth",
                home: "https://www.icloud.com",
                setup: "https://setup.icloud.com/setup/ws/1",
            }),
            "cn" => Ok(Self {
                auth_root: "https://idmsa.apple.com.cn",
                auth: "https://idmsa.apple.com.cn/appleauth/auth",
                home: "https://www.icloud.com.cn",
                setup: "https://setup.icloud.com.cn/setup/ws/1",
            }),
            _ => anyhow::bail!("Domain '{domain}' is not supported yet"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_domain_com_returns_correct_urls() {
        let ep = Endpoints::for_domain("com").unwrap();
        assert_eq!(ep.auth_root, "https://idmsa.apple.com");
        assert_eq!(ep.auth, "https://idmsa.apple.com/appleauth/auth");
        assert_eq!(ep.home, "https://www.icloud.com");
        assert_eq!(ep.setup, "https://setup.icloud.com/setup/ws/1");
    }

    #[test]
    fn for_domain_cn_returns_cn_urls() {
        let ep = Endpoints::for_domain("cn").unwrap();
        assert!(ep.auth_root.contains(".cn"), "auth_root should contain .cn");
        assert!(ep.auth.contains(".cn"), "auth should contain .cn");
        assert!(ep.home.contains(".cn"), "home should contain .cn");
        assert!(ep.setup.contains(".cn"), "setup should contain .cn");
    }

    #[test]
    fn for_domain_empty_string_returns_error() {
        let result = Endpoints::for_domain("");
        assert!(result.is_err());
    }

    #[test]
    fn for_domain_uk_returns_error() {
        let result = Endpoints::for_domain("uk");
        assert!(result.is_err());
    }

    #[test]
    fn error_message_mentions_unsupported_domain() {
        let err = Endpoints::for_domain("uk").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("uk"),
            "error message should mention the unsupported domain, got: {msg}"
        );
    }
}
