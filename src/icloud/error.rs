use thiserror::Error;

#[derive(Error, Debug)]
pub enum ICloudError {
    #[error("Connection error: {0}")]
    Connection(String),
    #[error(
        "iCloud service not activated ({code}): {reason}\n\n\
         This usually means one of:\n  \
         1. Advanced Data Protection (ADP) is enabled, which blocks third-party iCloud access.\n     \
            → Disable ADP in Settings > Apple Account > iCloud > Advanced Data Protection,\n     \
            or enable \"Access iCloud Data on the Web\" (Settings > Apple Account > iCloud).\n  \
         2. iCloud setup is incomplete.\n     \
            → Log into https://icloud.com/ and finish setting up your iCloud service."
    )]
    ServiceNotActivated { code: String, reason: String },
    #[error(transparent)]
    Http(Box<reqwest::Error>),
    #[error(transparent)]
    Io(Box<std::io::Error>),
    #[error(transparent)]
    Json(Box<serde_json::Error>),
}

impl From<reqwest::Error> for ICloudError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(Box::new(e))
    }
}

impl From<std::io::Error> for ICloudError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(Box::new(e))
    }
}

impl From<serde_json::Error> for ICloudError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(Box::new(e))
    }
}

const _: () = assert!(std::mem::size_of::<ICloudError>() <= 80);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_display_contains_message() {
        let err = ICloudError::Connection("timeout reached".into());
        let display = err.to_string();
        assert!(
            display.contains("timeout reached"),
            "expected display to contain the message, got: {display}"
        );
    }

    #[test]
    fn service_not_activated_display_mentions_code_reason_and_adp() {
        let err = ICloudError::ServiceNotActivated {
            code: "ZONE_NOT_FOUND".into(),
            reason: "service unavailable".into(),
        };
        let display = err.to_string();
        assert!(
            display.contains("ZONE_NOT_FOUND"),
            "expected display to contain the code, got: {display}"
        );
        assert!(
            display.contains("service unavailable"),
            "expected display to contain the reason, got: {display}"
        );
        assert!(
            display.contains("Advanced Data Protection"),
            "expected display to mention Advanced Data Protection, got: {display}"
        );
    }

    #[test]
    fn from_io_error_creates_io_variant() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: ICloudError = io_err.into();
        assert!(
            matches!(err, ICloudError::Io(_)),
            "expected Io variant, got: {err:?}"
        );
    }

    #[test]
    fn from_serde_json_error_creates_json_variant() {
        let json_err = serde_json::from_str::<serde_json::Value>("not valid json").unwrap_err();
        let err: ICloudError = json_err.into();
        assert!(
            matches!(err, ICloudError::Json(_)),
            "expected Json variant, got: {err:?}"
        );
    }
}
