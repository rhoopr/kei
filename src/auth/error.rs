use thiserror::Error;

/// Custom error types for iCloud authentication.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Failed login: {0}")]
    FailedLogin(String),

    #[error("Invalid authentication token: {0}")]
    InvalidToken(String),

    #[error("API error (HTTP {code}): {message}")]
    ApiError { code: u16, message: String },

    #[error("Two-factor authentication failed: {0}")]
    TwoFactorFailed(String),

    #[error("Apple service error ({code}): {message}")]
    ServiceError { code: String, message: String },

    #[error("Two-factor authentication is required (no code provided)")]
    TwoFactorRequired,

    #[error("Session lock held by another instance: {0}")]
    LockContention(String),

    #[error(transparent)]
    Http(Box<reqwest::Error>),

    #[error(transparent)]
    Io(Box<std::io::Error>),

    #[error(transparent)]
    Json(Box<serde_json::Error>),
}

impl From<reqwest::Error> for AuthError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(Box::new(e))
    }
}

impl From<std::io::Error> for AuthError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(Box::new(e))
    }
}

impl From<serde_json::Error> for AuthError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(Box::new(e))
    }
}

const _: () = assert!(std::mem::size_of::<AuthError>() <= 96);

impl AuthError {
    /// Check if this error indicates that 2FA is required but no code was provided.
    pub const fn is_two_factor_required(&self) -> bool {
        matches!(self, Self::TwoFactorRequired)
    }

    /// Check if this error indicates lock contention with another kei instance.
    pub const fn is_lock_contention(&self) -> bool {
        matches!(self, Self::LockContention(_))
    }

    /// Check if this error indicates Apple is rate limiting requests (HTTP 503).
    ///
    /// When rate limited, callers should back off rather than escalating to
    /// heavier auth flows (e.g. SRP) which would worsen the rate limit.
    pub fn is_rate_limited(&self) -> bool {
        match self {
            Self::ApiError { code, .. } => *code == 503,
            Self::ServiceError { code, .. } => code == "http_503",
            _ => false,
        }
    }

    /// Check if this error indicates a 421 Misdirected Request.
    ///
    /// HTTP 421 is an HTTP/2 routing issue where the connection was routed to
    /// the wrong partition server. The fix is to reset the connection pool and
    /// retry, NOT to re-authenticate.
    pub fn is_misdirected_request(&self) -> bool {
        match self {
            Self::ApiError { code, .. } => *code == 421,
            Self::ServiceError { code, .. } => code == "http_421",
            _ => false,
        }
    }

    /// Build a `ServiceError` with an enriched message for well-known Apple error codes.
    pub(crate) fn service_error(code: &str, raw_message: &str) -> Self {
        let upper = code.to_ascii_uppercase();
        let message = if upper == "ZONE_NOT_FOUND" || upper == "AUTHENTICATION_FAILED" {
            format!(
                "{raw_message}. Your iCloud account may not be fully set up — \
                 please sign in at https://icloud.com to complete setup."
            )
        } else if upper == "ACCESS_DENIED" {
            format!("{raw_message}. Please wait a few minutes then try again.")
        } else {
            raw_message.to_string()
        };
        Self::ServiceError {
            code: code.to_string(),
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_factor_required_is_detected() {
        assert!(AuthError::TwoFactorRequired.is_two_factor_required());
    }

    #[test]
    fn other_variants_are_not_two_factor_required() {
        assert!(!AuthError::FailedLogin("test".into()).is_two_factor_required());
        assert!(!AuthError::TwoFactorFailed("test".into()).is_two_factor_required());
        assert!(!AuthError::InvalidToken("test".into()).is_two_factor_required());
        assert!(!AuthError::LockContention("test".into()).is_two_factor_required());
        assert!(!AuthError::ApiError {
            code: 401,
            message: "test".into()
        }
        .is_two_factor_required());
        assert!(!AuthError::ServiceError {
            code: "test".into(),
            message: "test".into()
        }
        .is_two_factor_required());
    }

    #[test]
    fn lock_contention_is_detected() {
        assert!(AuthError::LockContention("test".into()).is_lock_contention());
    }

    #[test]
    fn other_variants_are_not_lock_contention() {
        assert!(!AuthError::FailedLogin("test".into()).is_lock_contention());
        assert!(!AuthError::TwoFactorRequired.is_lock_contention());
    }

    #[test]
    fn lock_contention_display() {
        let err = AuthError::LockContention("lock path".into());
        assert!(err.to_string().contains("lock path"));
    }

    #[test]
    fn two_factor_required_display() {
        let err = AuthError::TwoFactorRequired;
        assert_eq!(
            err.to_string(),
            "Two-factor authentication is required (no code provided)"
        );
    }

    #[test]
    fn failed_login_display() {
        let err = AuthError::FailedLogin("bad password".into());
        assert_eq!(err.to_string(), "Failed login: bad password");
    }

    #[test]
    fn invalid_token_display() {
        let err = AuthError::InvalidToken("expired".into());
        assert_eq!(err.to_string(), "Invalid authentication token: expired");
    }

    #[test]
    fn api_error_display() {
        let err = AuthError::ApiError {
            code: 403,
            message: "forbidden".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("forbidden"));
    }

    #[test]
    fn two_factor_failed_display() {
        let err = AuthError::TwoFactorFailed("wrong code".into());
        assert!(err.to_string().contains("wrong code"));
    }

    #[test]
    fn service_error_display() {
        let err = AuthError::ServiceError {
            code: "AUTH-401".into(),
            message: "Authentication required".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("AUTH-401"));
        assert!(msg.contains("Authentication required"));
    }

    #[test]
    fn service_error_is_not_two_factor_required() {
        let err = AuthError::ServiceError {
            code: "test".into(),
            message: "test".into(),
        };
        assert!(!err.is_two_factor_required());
    }

    #[test]
    fn service_error_enriches_zone_not_found() {
        let err = AuthError::service_error("ZONE_NOT_FOUND", "Zone not found");
        let msg = err.to_string();
        assert!(msg.contains("icloud.com"));
        assert!(msg.contains("set up"));
    }

    #[test]
    fn service_error_enriches_authentication_failed() {
        let err = AuthError::service_error("AUTHENTICATION_FAILED", "Auth failed");
        assert!(err.to_string().contains("set up"));
    }

    #[test]
    fn service_error_enriches_access_denied() {
        let err = AuthError::service_error("ACCESS_DENIED", "Denied");
        assert!(err.to_string().contains("wait a few minutes"));
    }

    #[test]
    fn service_error_passes_through_unknown_codes() {
        let err = AuthError::service_error("UNKNOWN_ERROR", "Something broke");
        assert!(err.to_string().contains("Something broke"));
        assert!(!err.to_string().contains("wait"));
        assert!(!err.to_string().contains("set up"));
    }

    #[test]
    fn api_error_503_is_rate_limited() {
        let err = AuthError::ApiError {
            code: 503,
            message: "HTTP 503 from Apple auth service".into(),
        };
        assert!(err.is_rate_limited());
    }

    #[test]
    fn service_error_http_503_is_rate_limited() {
        let err = AuthError::ServiceError {
            code: "http_503".into(),
            message: "Apple server error during validation (HTTP 503)".into(),
        };
        assert!(err.is_rate_limited());
    }

    #[test]
    fn api_error_other_codes_not_rate_limited() {
        for code in [401, 403, 421, 500, 502, 504] {
            let err = AuthError::ApiError {
                code,
                message: "test".into(),
            };
            assert!(
                !err.is_rate_limited(),
                "code {code} should not be rate limited"
            );
        }
    }

    #[test]
    fn service_error_other_codes_not_rate_limited() {
        for code in ["http_500", "http_502", "AUTH-401", "test"] {
            let err = AuthError::ServiceError {
                code: code.into(),
                message: "test".into(),
            };
            assert!(
                !err.is_rate_limited(),
                "code {code} should not be rate limited"
            );
        }
    }

    #[test]
    fn non_api_variants_not_rate_limited() {
        assert!(!AuthError::FailedLogin("test".into()).is_rate_limited());
        assert!(!AuthError::InvalidToken("test".into()).is_rate_limited());
        assert!(!AuthError::TwoFactorFailed("test".into()).is_rate_limited());
        assert!(!AuthError::TwoFactorRequired.is_rate_limited());
        assert!(!AuthError::LockContention("test".into()).is_rate_limited());
    }

    #[test]
    fn api_error_421_is_misdirected() {
        let err = AuthError::ApiError {
            code: 421,
            message: "Misdirected Request".into(),
        };
        assert!(err.is_misdirected_request());
    }

    #[test]
    fn service_error_http_421_is_misdirected() {
        let err = AuthError::ServiceError {
            code: "http_421".into(),
            message: "Misdirected Request during validation".into(),
        };
        assert!(err.is_misdirected_request());
    }

    #[test]
    fn api_error_other_codes_not_misdirected() {
        for code in [401, 403, 450, 500, 502, 503, 504] {
            let err = AuthError::ApiError {
                code,
                message: "test".into(),
            };
            assert!(
                !err.is_misdirected_request(),
                "code {code} should not be misdirected"
            );
        }
    }

    #[test]
    fn service_error_other_codes_not_misdirected() {
        for code in [
            "http_450", "http_500", "http_503", "rscd_401", "rscd_403", "rscd_421", "AUTH-421",
        ] {
            let err = AuthError::ServiceError {
                code: code.into(),
                message: "test".into(),
            };
            assert!(
                !err.is_misdirected_request(),
                "code {code} should not be misdirected"
            );
        }
    }

    #[test]
    fn non_api_variants_not_misdirected() {
        assert!(!AuthError::FailedLogin("test".into()).is_misdirected_request());
        assert!(!AuthError::InvalidToken("test".into()).is_misdirected_request());
        assert!(!AuthError::TwoFactorFailed("test".into()).is_misdirected_request());
        assert!(!AuthError::TwoFactorRequired.is_misdirected_request());
        assert!(!AuthError::LockContention("test".into()).is_misdirected_request());
    }

    #[test]
    fn misdirected_and_rate_limited_are_exclusive() {
        // 421 is misdirected, not rate limited
        let err_421 = AuthError::ApiError {
            code: 421,
            message: "test".into(),
        };
        assert!(err_421.is_misdirected_request());
        assert!(!err_421.is_rate_limited());

        // 503 is rate limited, not misdirected
        let err_503 = AuthError::ApiError {
            code: 503,
            message: "test".into(),
        };
        assert!(err_503.is_rate_limited());
        assert!(!err_503.is_misdirected_request());
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: AuthError = io_err.into();
        assert!(matches!(err, AuthError::Io(_)));
        assert!(err.to_string().contains("file missing"));
    }

    #[test]
    fn from_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("{{bad}").unwrap_err();
        let err: AuthError = json_err.into();
        assert!(matches!(err, AuthError::Json(_)));
    }
}
