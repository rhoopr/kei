use thiserror::Error;

#[derive(Error, Debug)]
pub enum ICloudError {
    #[error("Connection error: {0}")]
    Connection(String),
    #[error(
        "iCloud service not activated ({code}): {reason}\n\n\
         This usually means one of:\n  \
         1. Advanced Data Protection (ADP) is enabled, which blocks third-party iCloud access.\n     \
            To fix, change both settings on your iPhone/iPad:\n     \
            - Disable ADP: Settings > Apple ID > iCloud > Advanced Data Protection\n     \
            - Enable web access: Settings > Apple ID > iCloud > Access iCloud Data on the Web\n  \
         2. iCloud setup is incomplete.\n     \
            → Log into https://icloud.com/ and finish setting up your iCloud service."
    )]
    ServiceNotActivated { code: String, reason: String },
    /// CloudKit rejected the request with an auth-class HTTP status. Typically
    /// 401 (stale session), 403 (rotated routing cookie or an ADP edge case
    /// not caught earlier), or - rarely - another 4xx that maps to the same
    /// recovery path. The caller should invalidate any cached session data
    /// and re-authenticate with SRP before retrying.
    #[error("Session expired (HTTP {status} from CloudKit)")]
    SessionExpired { status: u16 },
    /// CloudKit returned HTTP 421 Misdirected Request. The HTTP/2 connection
    /// was routed to the wrong CloudKit partition; the caller should reset
    /// the connection pool and retry on a fresh connection.
    #[error("Misdirected Request (HTTP 421 from CloudKit)")]
    MisdirectedRequest,
    #[error(transparent)]
    Http(Box<reqwest::Error>),
    #[error(transparent)]
    Io(Box<std::io::Error>),
    #[error(transparent)]
    Json(Box<serde_json::Error>),
}

impl ICloudError {
    /// True if the error means kei should invalidate the session cache, force
    /// SRP re-authentication, and retry. Both `SessionExpired` (CloudKit
    /// 401/403) and `MisdirectedRequest` (persistent CloudKit 421) typically
    /// indicate stale session routing state that only SRP can re-mint.
    pub fn is_session_error(&self) -> bool {
        matches!(self, Self::SessionExpired { .. } | Self::MisdirectedRequest)
    }
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

    #[test]
    fn misdirected_request_is_distinct_variant() {
        let err = ICloudError::MisdirectedRequest;
        assert!(
            matches!(err, ICloudError::MisdirectedRequest),
            "dedicated variant so callers can reset pool and retry"
        );
        let display = err.to_string();
        assert!(display.contains("421"), "display mentions 421: {display}");
    }

    #[test]
    fn session_expired_is_distinct_variant() {
        let err = ICloudError::SessionExpired { status: 401 };
        assert!(
            matches!(err, ICloudError::SessionExpired { .. }),
            "dedicated variant so callers can trigger SRP re-auth"
        );
        let display = err.to_string();
        assert!(display.contains("401"), "display mentions 401: {display}");
    }

    #[test]
    fn session_expired_display_renders_actual_status() {
        // A 403 that maps to SessionExpired (e.g. bare CloudKit 403) must
        // surface as "HTTP 403" so the diagnostic matches the on-wire status.
        let err = ICloudError::SessionExpired { status: 403 };
        assert!(
            err.to_string().contains("HTTP 403"),
            "403 must render as HTTP 403, not HTTP 401: {err}"
        );
    }

    #[test]
    fn is_session_error_true_for_session_expired_and_misdirected() {
        assert!(ICloudError::SessionExpired { status: 401 }.is_session_error());
        assert!(ICloudError::SessionExpired { status: 403 }.is_session_error());
        assert!(ICloudError::MisdirectedRequest.is_session_error());
    }

    #[test]
    fn is_session_error_false_for_other_variants() {
        assert!(!ICloudError::Connection("x".into()).is_session_error());
        assert!(!ICloudError::ServiceNotActivated {
            code: "ADP".into(),
            reason: "y".into()
        }
        .is_session_error());
    }
}
