use thiserror::Error;

/// Typed download errors enabling retry classification.
///
/// The `is_retryable()` method distinguishes transient failures (server errors,
/// rate limits, content-length mismatches from truncated transfers) from
/// permanent ones (auth errors, disk failures) so the retry loop can abort early.
#[derive(Debug, Error)]
pub(crate) enum DownloadError {
    #[error("HTTP error {status} downloading {path}")]
    HttpStatus { status: u16, path: String },

    #[error("Content-length mismatch for {path}: expected {expected} bytes, received {received}")]
    ContentLengthMismatch {
        path: String,
        expected: u64,
        received: u64,
    },

    #[error("Disk error: {0}")]
    Disk(Box<std::io::Error>),

    #[error("HTTP error downloading {path} (status={status}, content_length={content_length:?}, bytes_so_far={bytes_written}): {source}")]
    Http {
        source: Box<dyn std::error::Error + Send + Sync>,
        path: String,
        status: u16,
        content_length: Option<u64>,
        bytes_written: u64,
    },

    #[error("Invalid content for {path}: {reason}")]
    InvalidContent { path: String, reason: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        Self::Disk(Box::new(e))
    }
}

// Verify boxing keeps enum small — guards against regressions from adding unboxed large variants.
const _: () = assert!(std::mem::size_of::<DownloadError>() <= 88);

impl DownloadError {
    /// Whether this error is transient and worth retrying.
    pub fn is_retryable(&self) -> bool {
        match self {
            DownloadError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            DownloadError::ContentLengthMismatch { .. }
            | DownloadError::InvalidContent { .. }
            | DownloadError::Http { .. } => true,
            DownloadError::Disk(_) | DownloadError::Other(_) => false,
        }
    }

    /// Whether this error indicates the session has expired.
    ///
    /// HTTP 401 (Unauthorized) and 403 (Forbidden) typically indicate that
    /// the iCloud session token has been invalidated server-side. The caller
    /// should re-authenticate and retry.
    pub fn is_session_expired(&self) -> bool {
        matches!(
            self,
            DownloadError::HttpStatus {
                status: 401 | 403,
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_404_not_retryable() {
        let e = DownloadError::HttpStatus {
            status: 404,
            path: "x".into(),
        };
        assert!(!e.is_retryable());
    }

    #[test]
    fn test_http_401_not_retryable() {
        let e = DownloadError::HttpStatus {
            status: 401,
            path: "x".into(),
        };
        assert!(!e.is_retryable());
    }

    #[test]
    fn test_http_403_not_retryable() {
        let e = DownloadError::HttpStatus {
            status: 403,
            path: "x".into(),
        };
        assert!(!e.is_retryable());
    }

    #[test]
    fn test_http_429_retryable() {
        let e = DownloadError::HttpStatus {
            status: 429,
            path: "x".into(),
        };
        assert!(e.is_retryable());
    }

    #[test]
    fn test_http_500_retryable() {
        let e = DownloadError::HttpStatus {
            status: 500,
            path: "x".into(),
        };
        assert!(e.is_retryable());
    }

    #[test]
    fn test_http_503_retryable() {
        let e = DownloadError::HttpStatus {
            status: 503,
            path: "x".into(),
        };
        assert!(e.is_retryable());
    }

    #[test]
    fn test_disk_not_retryable() {
        let e = DownloadError::Disk(Box::new(std::io::Error::other("disk full")));
        assert!(!e.is_retryable());
    }

    #[test]
    fn test_other_not_retryable() {
        let e = DownloadError::Other(anyhow::anyhow!("unknown"));
        assert!(!e.is_retryable());
    }

    #[test]
    fn test_http_connection_error_retryable() {
        // Create a reqwest::Error by requesting an unreachable address
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(reqwest::Client::new().get("http://127.0.0.1:1").send())
            .unwrap_err();
        let e = DownloadError::Http {
            source: Box::new(err),
            path: "x".into(),
            status: 0,
            content_length: None,
            bytes_written: 0,
        };
        assert!(e.is_retryable());
    }

    #[test]
    fn test_http_401_is_session_expired() {
        let e = DownloadError::HttpStatus {
            status: 401,
            path: "x".into(),
        };
        assert!(e.is_session_expired());
    }

    #[test]
    fn test_http_403_is_session_expired() {
        let e = DownloadError::HttpStatus {
            status: 403,
            path: "x".into(),
        };
        assert!(e.is_session_expired());
    }

    #[test]
    fn test_http_500_not_session_expired() {
        let e = DownloadError::HttpStatus {
            status: 500,
            path: "x".into(),
        };
        assert!(!e.is_session_expired());
    }

    #[test]
    fn test_http_404_not_session_expired() {
        let e = DownloadError::HttpStatus {
            status: 404,
            path: "x".into(),
        };
        assert!(!e.is_session_expired());
    }

    #[test]
    fn test_content_length_mismatch_retryable() {
        let e = DownloadError::ContentLengthMismatch {
            path: "video.mov".into(),
            expected: 1_073_741_824,
            received: 1_060_000_000,
        };
        assert!(e.is_retryable());
        assert!(!e.is_session_expired());
    }

    #[test]
    fn test_disk_not_session_expired() {
        let e = DownloadError::Disk(Box::new(std::io::Error::other("disk full")));
        assert!(!e.is_session_expired());
    }

    #[test]
    fn display_content_length_mismatch_includes_expected_and_received() {
        // Arrange
        let e = DownloadError::ContentLengthMismatch {
            path: "photo.jpg".into(),
            expected: 5000,
            received: 3200,
        };

        // Act
        let msg = e.to_string();

        // Assert
        assert!(msg.contains("photo.jpg"), "Display should include the path");
        assert!(
            msg.contains("5000"),
            "Display should include expected bytes"
        );
        assert!(
            msg.contains("3200"),
            "Display should include received bytes"
        );
    }

    #[test]
    fn display_http_status_includes_status_and_path() {
        // Arrange
        let e = DownloadError::HttpStatus {
            status: 502,
            path: "/photos/abc.heic".into(),
        };

        // Act
        let msg = e.to_string();

        // Assert
        assert!(
            msg.contains("502"),
            "Display should include the status code"
        );
        assert!(
            msg.contains("/photos/abc.heic"),
            "Display should include the path"
        );
    }

    #[test]
    fn display_disk_includes_io_error_message() {
        // Arrange
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");
        let e = DownloadError::Disk(Box::new(io_err));

        // Act
        let msg = e.to_string();

        // Assert
        assert!(
            msg.contains("permission denied"),
            "Display should include the underlying IO error message"
        );
    }

    #[test]
    fn is_session_expired_http_variant_401_returns_false() {
        // The Http variant (mid-stream error) is distinct from HttpStatus.
        // Even with status 401, Http is always retryable and never session-expired.
        // Arrange
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(reqwest::Client::new().get("http://127.0.0.1:1").send())
            .unwrap_err();
        let e = DownloadError::Http {
            source: Box::new(err),
            path: "x".into(),
            status: 401,
            content_length: None,
            bytes_written: 0,
        };

        // Act / Assert
        assert!(
            !e.is_session_expired(),
            "Http variant should never be treated as session expired"
        );
    }

    #[test]
    fn is_session_expired_http_variant_403_returns_false() {
        // Arrange
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(reqwest::Client::new().get("http://127.0.0.1:1").send())
            .unwrap_err();
        let e = DownloadError::Http {
            source: Box::new(err),
            path: "x".into(),
            status: 403,
            content_length: None,
            bytes_written: 0,
        };

        // Act / Assert
        assert!(
            !e.is_session_expired(),
            "Http variant should never be treated as session expired"
        );
    }

    #[test]
    fn download_error_implements_std_error_trait() {
        // Verify DownloadError can be used as an anyhow::Error source,
        // which requires implementing std::error::Error.
        // Arrange
        let e = DownloadError::HttpStatus {
            status: 500,
            path: "test.jpg".into(),
        };

        // Act — wrap in anyhow to prove it implements std::error::Error + Send + Sync
        let anyhow_err: anyhow::Error = e.into();

        // Assert
        assert!(
            anyhow_err.to_string().contains("500"),
            "Wrapped error should preserve the Display output"
        );
    }

    #[test]
    fn other_variant_not_retryable_and_not_session_expired() {
        // Arrange
        let e = DownloadError::Other(anyhow::anyhow!("unexpected parse failure"));

        // Act / Assert
        assert!(!e.is_retryable(), "Other variant should not be retryable");
        assert!(
            !e.is_session_expired(),
            "Other variant should not be session expired"
        );
    }

    #[test]
    fn test_invalid_content_retryable() {
        let e = DownloadError::InvalidContent {
            path: "photo.heic".into(),
            reason: "file contains HTML".into(),
        };
        assert!(e.is_retryable());
        assert!(!e.is_session_expired());
    }

    #[test]
    fn display_invalid_content_includes_path_and_reason() {
        let e = DownloadError::InvalidContent {
            path: "photo.jpg".into(),
            reason: "file header does not match expected format for .jpg".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("photo.jpg"));
        assert!(msg.contains("does not match"));
    }

    #[test]
    fn other_variant_display_is_transparent() {
        // The #[error(transparent)] attribute means Display delegates to the inner error.
        // Arrange
        let inner_msg = "json decode failed";
        let e = DownloadError::Other(anyhow::anyhow!(inner_msg));

        // Act
        let msg = e.to_string();

        // Assert
        assert_eq!(
            msg, inner_msg,
            "Other variant Display should match inner error"
        );
    }
}
