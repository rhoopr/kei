use thiserror::Error;

#[derive(Error, Debug)]
pub enum ICloudError {
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
