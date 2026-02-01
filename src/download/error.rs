use thiserror::Error;

/// Typed download errors enabling retry classification.
///
/// The `is_retryable()` method distinguishes transient failures (server errors,
/// rate limits, checksum mismatches from truncated transfers) from permanent
/// ones (auth errors, disk failures) so the retry loop can abort early.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("HTTP error {status} downloading {path}")]
    HttpStatus { status: u16, path: String },

    #[error("Checksum mismatch for {0}")]
    ChecksumMismatch(String),

    #[error("Disk error: {0}")]
    Disk(#[from] std::io::Error),

    #[error("HTTP error downloading {path} (status={status}, content_length={content_length:?}, bytes_so_far={bytes_written}): {source}")]
    Http {
        source: reqwest::Error,
        path: String,
        status: u16,
        content_length: Option<u64>,
        bytes_written: u64,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl DownloadError {
    /// Whether this error is transient and worth retrying.
    ///
    /// Checksum mismatches are retryable because they typically indicate a
    /// truncated transfer or expired CDN URL, not actual data corruption.
    pub fn is_retryable(&self) -> bool {
        match self {
            DownloadError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
            DownloadError::ChecksumMismatch(_) => true,
            DownloadError::Http { .. } => true,
            DownloadError::Disk(_) => false,
            DownloadError::Other(_) => false,
        }
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
    fn test_checksum_mismatch_retryable() {
        let e = DownloadError::ChecksumMismatch("x".into());
        assert!(e.is_retryable());
    }

    #[test]
    fn test_disk_not_retryable() {
        let e = DownloadError::Disk(std::io::Error::other("disk full"));
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
            source: err,
            path: "x".into(),
            status: 0,
            content_length: None,
            bytes_written: 0,
        };
        assert!(e.is_retryable());
    }
}
