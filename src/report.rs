//! JSON sync report generation.
//!
//! Writes a structured JSON summary after each sync cycle for machine consumption
//! (monitoring tools, Home Assistant, webhooks).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::download::SyncStats;

/// Top-level JSON report written after each sync cycle.
#[derive(Debug, Serialize)]
pub(crate) struct SyncReport {
    /// Schema version for forward compatibility.
    pub version: &'static str,
    /// kei binary version.
    pub kei_version: &'static str,
    /// ISO 8601 timestamp of when the report was generated.
    pub timestamp: String,
    /// Sync outcome: "success", "partial_failure", or "session_expired".
    pub status: String,
    /// CLI/config options the sync was invoked with.
    pub options: RunOptions,
    /// Accumulated sync statistics.
    pub stats: SyncStats,
}

/// User-facing options captured from the resolved Config. No secrets.
#[derive(Debug, Serialize)]
pub(crate) struct RunOptions {
    pub username: String,
    pub directory: PathBuf,
    pub folder_structure: String,
    pub size: String,
    pub live_photo_mode: String,
    pub live_photo_size: String,
    pub file_match_policy: String,
    pub albums: Vec<String>,
    pub library: String,
    pub skip_videos: bool,
    pub skip_photos: bool,
    pub set_exif_datetime: bool,
    pub threads_num: u16,
    pub no_incremental: bool,
    pub dry_run: bool,
}

impl RunOptions {
    /// Build from the resolved Config. Only includes user-facing settings.
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        Self {
            username: config.username.clone(),
            directory: config.directory.clone(),
            folder_structure: config.folder_structure.clone(),
            size: format!("{:?}", config.size).to_lowercase(),
            live_photo_mode: format!("{:?}", config.live_photo_mode).to_lowercase(),
            live_photo_size: format!("{:?}", config.live_photo_size).to_lowercase(),
            file_match_policy: format!("{:?}", config.file_match_policy).to_lowercase(),
            albums: config.albums.to_vec(),
            library: format!("{:?}", config.library).to_lowercase(),
            skip_videos: config.skip_videos,
            skip_photos: config.skip_photos,
            set_exif_datetime: config.set_exif_datetime,
            threads_num: config.threads_num,
            no_incremental: config.no_incremental,
            dry_run: config.dry_run,
        }
    }
}

/// Write a JSON report to the given path atomically (temp file + rename).
pub(crate) fn write_report(path: &Path, report: &SyncReport) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(report)?;

    // Write to a temp file in the same directory, then rename for atomicity.
    let parent = path.parent().unwrap_or(Path::new("."));
    let temp_path = parent.join(format!(".kei-report-{}.tmp", std::process::id()));

    std::fs::write(&temp_path, json.as_bytes())?;
    std::fs::rename(&temp_path, path).or_else(|_| {
        // Cross-device rename fallback: copy + remove
        std::fs::copy(&temp_path, path)?;
        std::fs::remove_file(&temp_path).ok();
        Ok::<(), std::io::Error>(())
    })?;

    tracing::debug!(path = %path.display(), "Wrote JSON report");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::SkipBreakdown;

    #[test]
    fn report_serialization_roundtrip() {
        let report = SyncReport {
            version: "1",
            kei_version: "0.7.12",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "success".to_string(),
            options: RunOptions {
                username: "user@example.com".to_string(),
                directory: PathBuf::from("/photos"),
                folder_structure: "{:%Y/%m/%d}".to_string(),
                size: "original".to_string(),
                live_photo_mode: "original".to_string(),
                live_photo_size: "original".to_string(),
                file_match_policy: "name-size-dedup".to_string(),
                albums: vec!["Favorites".to_string()],
                library: "personal".to_string(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: true,
                threads_num: 4,
                no_incremental: false,
                dry_run: false,
            },
            stats: SyncStats {
                assets_seen: 400,
                downloaded: 50,
                failed: 2,
                skipped: SkipBreakdown {
                    by_state: 300,
                    on_disk: 30,
                    by_media_type: 10,
                    by_date_range: 5,
                    ..SkipBreakdown::default()
                },
                bytes_downloaded: 1_200_000_000,
                disk_bytes_written: 1_300_000_000,
                elapsed_secs: 263.5,
                ..SyncStats::default()
            },
        };

        let json = serde_json::to_string_pretty(&report).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

        assert_eq!(parsed["version"], "1");
        assert_eq!(parsed["status"], "success");
        assert_eq!(parsed["stats"]["downloaded"], 50);
        assert_eq!(parsed["stats"]["skipped"]["by_state"], 300);
        assert_eq!(parsed["options"]["username"], "user@example.com");
        assert!(parsed["options"]["set_exif_datetime"]
            .as_bool()
            .unwrap_or(false));
    }

    #[test]
    fn write_report_creates_valid_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");

        let report = SyncReport {
            version: "1",
            kei_version: "0.7.12",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "success".to_string(),
            options: RunOptions {
                username: "test@example.com".to_string(),
                directory: PathBuf::from("/tmp/photos"),
                folder_structure: "{:%Y/%m/%d}".to_string(),
                size: "original".to_string(),
                live_photo_mode: "original".to_string(),
                live_photo_size: "original".to_string(),
                file_match_policy: "name-size-dedup".to_string(),
                albums: vec![],
                library: "personal".to_string(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                threads_num: 3,
                no_incremental: false,
                dry_run: false,
            },
            stats: SyncStats::default(),
        };

        write_report(&path, &report).expect("write_report");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["version"], "1");
        assert_eq!(parsed["options"]["username"], "test@example.com");
    }
}
