use thiserror::Error;

/// Custom error types for iCloud authentication.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum AuthError {
    #[error("Failed login: {0}")]
    FailedLogin(String),

    #[error("Invalid authentication token: {0}")]
    InvalidToken(String),

    #[error("API error (HTTP {code}): {message}")]
    ApiError { code: u16, message: String },

    #[error("Two-factor authentication failed: {0}")]
    TwoFactorFailed(String),

    #[error("Service not activated: {0}")]
    ServiceNotActivated(String),

    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
