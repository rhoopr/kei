use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum ICloudError {
    #[error("API response error: {reason} (code: {code})")]
    ApiResponse { reason: String, code: String },
    #[error("Service not activated: {0}")]
    ServiceNotActivated(String),
    #[error("Failed login: {0}")]
    FailedLogin(String),
    #[error("2FA required")]
    TwoFactorRequired,
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Photo library not finished indexing")]
    IndexingNotFinished,
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
