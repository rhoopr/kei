use thiserror::Error;

/// Format an optional list of security-key names for display.
/// Returns " (YubiKey 5C, Passkey-1)" when names are known, or an empty
/// string when Apple did not disclose them.
fn format_fido_keys(keys: &[String]) -> String {
    if keys.is_empty() {
        String::new()
    } else {
        format!(" ({})", keys.join(", "))
    }
}

/// Apple service error code for an account lock that requires operator action.
pub(crate) const APPLE_ACCOUNT_LOCKED_CODE: &str = "-20209";
/// Apple service error code for credentials rejected by SRP complete.
pub(crate) const APPLE_INVALID_CREDENTIALS_CODE: &str = "-20101";

const ACCOUNT_LOCKED_RECOVERY: &str =
    "Visit https://iforgot.apple.com, update kei's stored password, then restart kei.";
const INVALID_CREDENTIALS_RECOVERY: &str = "Update kei's stored password, then restart kei.";

/// Custom error types for iCloud authentication.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Could not log in to iCloud: {0}")]
    FailedLogin(String),

    #[error("The saved iCloud login token is no longer valid: {0}")]
    InvalidToken(String),

    #[error("Apple authentication returned HTTP {code}: {message}")]
    ApiError { code: u16, message: String },

    #[error("Two-factor authentication did not complete: {0}")]
    TwoFactorFailed(String),

    #[error("Apple authentication service returned {code}: {message}")]
    ServiceError { code: String, message: String },

    #[error(
        "Apple authentication needs your attention ({code}): {message}. {}",
        terminal_apple_auth_recovery(code)
    )]
    TerminalAppleAuth { code: String, message: String },

    #[error(
        "Two-factor authentication is required. Run `kei login get-code`, then `kei login submit-code <CODE>`."
    )]
    TwoFactorRequired,

    /// The Apple ID has FIDO/WebAuthn hardware security keys registered.
    /// Apple signals this via `fsaChallenge` / `keyNames` in the 2FA
    /// challenge body. CloudKit rejects sessions minted through this path
    /// with "no auth method found", so kei bails early rather than let the
    /// user hit that downstream failure and loop through re-auth.
    #[error(
        "This Apple ID has FIDO/WebAuthn hardware security keys registered{}. \
         kei cannot use Apple accounts that require security keys yet (see issue #221).\n\n\
         To use kei with this account, remove the security keys at:\n  \
         Settings > Apple ID & iCloud > Sign-In & Security > Security Keys",
        format_fido_keys(key_names)
    )]
    FidoNotSupported { key_names: Vec<String> },

    #[error("Another kei process is using the iCloud session: {0}")]
    LockContention(String),

    #[error(
        "The local iCloud session was reset while this process was idle. Restart kei to load fresh authentication state."
    )]
    SessionReset,

    #[error(
        "Authenticated account identity changed or became unavailable; stopping before further state or provider work. Restart with the intended account."
    )]
    AccountIdentityChanged,

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

    pub(crate) const fn is_account_identity_changed(&self) -> bool {
        matches!(self, Self::AccountIdentityChanged)
    }

    pub(crate) const fn is_session_reset(&self) -> bool {
        matches!(self, Self::SessionReset)
    }

    /// True when Apple returned a terminal authentication state that should
    /// not be treated like a transient auth failure by supervisors.
    pub const fn is_terminal_apple_auth(&self) -> bool {
        matches!(self, Self::TerminalAppleAuth { .. })
    }

    /// True when Apple's auth surface is returning a transient failure
    /// class: explicit rate limiting (HTTP 429, 503) or any other 5xx
    /// ("Apple is having trouble" from the caller's perspective).
    ///
    /// Callers use this to decide between "wait a few minutes, do not
    /// escalate to SRP" and a hard failure. The SRP retry loop already
    /// absorbs short blips; this predicate fires after retries are
    /// exhausted to add back-off guidance to the surfaced error.
    pub fn is_transient_apple_failure(&self) -> bool {
        match self {
            Self::ApiError { code, .. } => *code == 429 || (500..600).contains(code),
            Self::ServiceError { code, .. } => code
                .strip_prefix("http_")
                .and_then(|s| s.parse::<u16>().ok())
                .is_some_and(|c| c == 429 || (500..600).contains(&c)),
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

    pub(crate) fn terminal_apple_auth(code: &str, raw_message: &str) -> Self {
        Self::TerminalAppleAuth {
            code: code.to_string(),
            message: raw_message.to_string(),
        }
    }
}

pub(crate) fn is_terminal_apple_auth_code(code: &str) -> bool {
    code == APPLE_ACCOUNT_LOCKED_CODE || code == APPLE_INVALID_CREDENTIALS_CODE
}

fn terminal_apple_auth_recovery(code: &str) -> &'static str {
    if code == APPLE_ACCOUNT_LOCKED_CODE {
        ACCOUNT_LOCKED_RECOVERY
    } else {
        INVALID_CREDENTIALS_RECOVERY
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
        assert!(
            !AuthError::terminal_apple_auth(APPLE_ACCOUNT_LOCKED_CODE, "locked")
                .is_two_factor_required()
        );
        assert!(
            !AuthError::ApiError {
                code: 401,
                message: "test".into()
            }
            .is_two_factor_required()
        );
        assert!(
            !AuthError::ServiceError {
                code: "test".into(),
                message: "test".into()
            }
            .is_two_factor_required()
        );
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
    fn terminal_apple_auth_is_detected() {
        let err = AuthError::terminal_apple_auth(APPLE_ACCOUNT_LOCKED_CODE, "Account locked");
        assert!(err.is_terminal_apple_auth());
    }

    #[test]
    fn other_variants_are_not_terminal_apple_auth() {
        assert!(!AuthError::FailedLogin("test".into()).is_terminal_apple_auth());
        assert!(!AuthError::TwoFactorRequired.is_terminal_apple_auth());
        assert!(
            !AuthError::ApiError {
                code: 403,
                message: "forbidden".into()
            }
            .is_terminal_apple_auth()
        );
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
            "Two-factor authentication is required. Run `kei login get-code`, then `kei login submit-code <CODE>`."
        );
    }

    #[test]
    fn failed_login_display() {
        let err = AuthError::FailedLogin("bad password".into());
        assert_eq!(err.to_string(), "Could not log in to iCloud: bad password");
    }

    #[test]
    fn invalid_token_display() {
        let err = AuthError::InvalidToken("expired".into());
        assert_eq!(
            err.to_string(),
            "The saved iCloud login token is no longer valid: expired"
        );
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
    fn terminal_apple_auth_display_is_actionable() {
        let err = AuthError::terminal_apple_auth(APPLE_ACCOUNT_LOCKED_CODE, "Account locked");
        let msg = err.to_string();
        assert!(msg.contains(APPLE_ACCOUNT_LOCKED_CODE));
        assert!(msg.contains("Account locked"));
        assert!(msg.contains("iforgot.apple.com"));
        assert!(msg.contains("update kei's stored password"));
        assert!(msg.contains("restart kei"));
    }

    #[test]
    fn terminal_invalid_credentials_display_skips_iforgot() {
        let err =
            AuthError::terminal_apple_auth(APPLE_INVALID_CREDENTIALS_CODE, "Incorrect password");
        let msg = err.to_string();
        assert!(msg.contains(APPLE_INVALID_CREDENTIALS_CODE));
        assert!(msg.contains("Incorrect password"));
        assert!(!msg.contains("iforgot.apple.com"));
        assert!(msg.contains("Update kei's stored password"));
        assert!(msg.contains("restart kei"));
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
    fn api_error_429_and_5xx_are_transient() {
        for code in [429, 500, 502, 503, 504] {
            let err = AuthError::ApiError {
                code,
                message: "test".into(),
            };
            assert!(
                err.is_transient_apple_failure(),
                "code {code} should be transient"
            );
        }
    }

    #[test]
    fn service_error_http_429_and_5xx_are_transient() {
        for code in ["http_429", "http_500", "http_502", "http_503", "http_504"] {
            let err = AuthError::ServiceError {
                code: code.into(),
                message: "test".into(),
            };
            assert!(
                err.is_transient_apple_failure(),
                "code {code} should be transient"
            );
        }
    }

    #[test]
    fn api_error_non_transient_codes_are_not_transient() {
        for code in [400, 401, 403, 409, 412, 421, 450] {
            let err = AuthError::ApiError {
                code,
                message: "test".into(),
            };
            assert!(
                !err.is_transient_apple_failure(),
                "code {code} should not be transient"
            );
        }
    }

    #[test]
    fn service_error_non_http_code_is_not_transient() {
        for code in ["AUTH-401", "rscd_401", "rscd_403", "ZONE_NOT_FOUND"] {
            let err = AuthError::ServiceError {
                code: code.into(),
                message: "test".into(),
            };
            assert!(
                !err.is_transient_apple_failure(),
                "code {code} should not be transient"
            );
        }
    }

    #[test]
    fn non_api_variants_are_not_transient() {
        assert!(!AuthError::FailedLogin("test".into()).is_transient_apple_failure());
        assert!(!AuthError::InvalidToken("test".into()).is_transient_apple_failure());
        assert!(!AuthError::TwoFactorFailed("test".into()).is_transient_apple_failure());
        assert!(!AuthError::TwoFactorRequired.is_transient_apple_failure());
        assert!(!AuthError::LockContention("test".into()).is_transient_apple_failure());
        assert!(
            !AuthError::terminal_apple_auth(APPLE_ACCOUNT_LOCKED_CODE, "locked")
                .is_transient_apple_failure()
        );
    }

    #[test]
    fn api_error_5xx_range_boundary_599_is_transient_600_is_not() {
        let err_599 = AuthError::ApiError {
            code: 599,
            message: "test".into(),
        };
        assert!(
            err_599.is_transient_apple_failure(),
            "599 is within 500..600 and should be transient"
        );

        let err_600 = AuthError::ApiError {
            code: 600,
            message: "test".into(),
        };
        assert!(
            !err_600.is_transient_apple_failure(),
            "600 is outside 500..600 and should not be transient"
        );

        let err_499 = AuthError::ApiError {
            code: 499,
            message: "test".into(),
        };
        assert!(
            !err_499.is_transient_apple_failure(),
            "499 is below the 5xx range and should not be transient"
        );
    }

    #[test]
    fn service_error_enrichment_is_case_insensitive() {
        let lowercase = AuthError::service_error("zone_not_found", "Zone not found");
        assert!(
            lowercase.to_string().contains("icloud.com"),
            "lowercase code should still trigger enrichment"
        );

        let mixed = AuthError::service_error("Zone_Not_Found", "Zone not found");
        assert!(
            mixed.to_string().contains("icloud.com"),
            "mixed-case code should still trigger enrichment"
        );

        let lowercase_auth = AuthError::service_error("authentication_failed", "Auth failed");
        assert!(
            lowercase_auth.to_string().contains("set up"),
            "lowercase authentication_failed should enrich"
        );

        let lowercase_denied = AuthError::service_error("access_denied", "Denied");
        assert!(
            lowercase_denied.to_string().contains("wait a few minutes"),
            "lowercase access_denied should enrich"
        );
    }

    #[test]
    fn service_error_enrichment_requires_exact_code_no_whitespace() {
        let trailing_space = AuthError::service_error("ZONE_NOT_FOUND ", "Zone not found");
        assert!(
            !trailing_space.to_string().contains("icloud.com"),
            "trailing space should not trigger enrichment (exact match after uppercasing)"
        );

        let prefixed = AuthError::service_error("X_ZONE_NOT_FOUND", "Zone not found");
        assert!(
            !prefixed.to_string().contains("icloud.com"),
            "prefixed code should not trigger enrichment"
        );
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
        assert!(
            !AuthError::terminal_apple_auth(APPLE_ACCOUNT_LOCKED_CODE, "locked")
                .is_misdirected_request()
        );
    }

    #[test]
    fn misdirected_and_transient_are_exclusive() {
        // 421 is misdirected, not a transient-failure (it has a dedicated
        // recovery path: reset the HTTP/2 pool, do not retry as-is).
        let err_421 = AuthError::ApiError {
            code: 421,
            message: "test".into(),
        };
        assert!(err_421.is_misdirected_request());
        assert!(!err_421.is_transient_apple_failure());

        // 503 is transient, not misdirected
        let err_503 = AuthError::ApiError {
            code: 503,
            message: "test".into(),
        };
        assert!(err_503.is_transient_apple_failure());
        assert!(!err_503.is_misdirected_request());
    }

    #[test]
    fn fido_not_supported_display_with_key_names() {
        let err = AuthError::FidoNotSupported {
            key_names: vec!["YubiKey 5C".into(), "Passkey-Home".into()],
        };
        let msg = err.to_string();
        assert!(
            msg.contains("YubiKey 5C"),
            "display should name the registered key, got: {msg}"
        );
        assert!(
            msg.contains("Passkey-Home"),
            "display should list all keys, got: {msg}"
        );
        assert!(
            msg.contains("#221"),
            "display should reference the tracking issue, got: {msg}"
        );
        assert!(
            msg.contains("Sign-In & Security"),
            "display should point to the settings path, got: {msg}"
        );
    }

    #[test]
    fn fido_not_supported_display_without_key_names() {
        // Apple's response may omit keyNames (empty array) while still
        // signaling FIDO via fsaChallenge. The message must still render
        // cleanly without a trailing empty parenthesis.
        let err = AuthError::FidoNotSupported { key_names: vec![] };
        let msg = err.to_string();
        assert!(
            !msg.contains("()"),
            "empty key list should not render empty parens, got: {msg}"
        );
        assert!(
            msg.contains("security keys registered"),
            "base message must still be present, got: {msg}"
        );
        assert!(msg.contains("#221"), "issue link required, got: {msg}");
    }

    #[test]
    fn fido_not_supported_is_not_transient() {
        // FIDO detection is a terminal error: no amount of retry will help
        // until the user removes the keys from their account.
        let err = AuthError::FidoNotSupported {
            key_names: vec!["YubiKey".into()],
        };
        assert!(!err.is_transient_apple_failure());
        assert!(!err.is_misdirected_request());
        assert!(!err.is_two_factor_required());
        assert!(!err.is_lock_contention());
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
