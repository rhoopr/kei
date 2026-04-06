use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct HealthStatus {
    pub last_sync_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

impl HealthStatus {
    pub fn new() -> Self {
        Self {
            last_sync_at: None,
            last_success_at: None,
            consecutive_failures: 0,
            last_error: None,
        }
    }

    pub fn record_success(&mut self) {
        let now = Utc::now();
        self.last_sync_at = Some(now);
        self.last_success_at = Some(now);
        self.consecutive_failures = 0;
        self.last_error = None;
    }

    pub fn record_failure(&mut self, error: &str) {
        self.last_sync_at = Some(Utc::now());
        self.consecutive_failures += 1;
        self.last_error = Some(error.to_string());
    }

    /// Write health status to `health.json` in the given directory.
    /// Errors are logged but never propagated — health reporting must not
    /// interfere with the sync loop.
    pub fn write(&self, dir: &Path) {
        let path = dir.join("health.json");
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!(error = %e, path = %path.display(), "Failed to write health file");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize health status");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_no_timestamps() {
        let h = HealthStatus::new();
        assert!(h.last_sync_at.is_none());
        assert!(h.last_success_at.is_none());
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.last_error.is_none());
    }

    #[test]
    fn record_success_sets_timestamps_and_resets_failures() {
        let mut h = HealthStatus::new();
        h.record_failure("boom");
        h.record_failure("boom again");
        h.record_success();

        assert!(h.last_sync_at.is_some());
        assert!(h.last_success_at.is_some());
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.last_error.is_none());
    }

    #[test]
    fn record_failure_increments_and_preserves_last_success() {
        let mut h = HealthStatus::new();
        h.record_success();
        let success_time = h.last_success_at;

        h.record_failure("err1");
        assert_eq!(h.consecutive_failures, 1);
        assert_eq!(h.last_error.as_deref(), Some("err1"));
        assert_eq!(h.last_success_at, success_time);

        h.record_failure("err2");
        assert_eq!(h.consecutive_failures, 2);
        assert_eq!(h.last_error.as_deref(), Some("err2"));
    }

    #[test]
    fn write_creates_valid_json() {
        let dir = tempfile::tempdir().unwrap();

        let mut h = HealthStatus::new();
        h.record_success();
        h.write(dir.path());

        let contents = std::fs::read_to_string(dir.path().join("health.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert!(parsed["last_sync_at"].is_string());
        assert!(parsed["last_success_at"].is_string());
        assert_eq!(parsed["consecutive_failures"], 0);
        assert!(parsed["last_error"].is_null());
    }

    #[test]
    fn write_nonexistent_dir_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let h = HealthStatus::new();
        h.write(&dir.path().join("nonexistent_subdir"));
    }
}
