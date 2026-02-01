use thiserror::Error;

use crate::icloud::error::ICloudError;

#[derive(Debug, Error)]
#[allow(dead_code)] // not all variants constructed yet; part of public error API
pub enum PhotosError {
    #[error("Missing required field '{field}' on asset {asset_id}")]
    MissingField { asset_id: String, field: String },

    #[error("Malformed API response: {0}")]
    MalformedResponse(String),

    #[error(transparent)]
    ICloud(#[from] ICloudError),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
