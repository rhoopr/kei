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
