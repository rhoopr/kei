//! JSON sync report generation.
//!
//! Writes a structured JSON summary after each sync cycle for machine consumption
//! (monitoring tools, Home Assistant, webhooks).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::download::SyncStats;
use crate::state::AssetRecord;

/// Cap on `failed_assets` entries so an account with hundreds of thousands of
/// failures doesn't blow up the report JSON. The tail count is preserved in
/// `failed_assets_truncated`.
pub(crate) const FAILED_ASSETS_CAP: usize = 200;

/// Structured per-asset failure entry for operators to consume without
/// grepping the log. Populated from the state DB after the sync cycle
/// completes so it reflects the final `status='failed'` set.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FailedAssetEntry {
    pub id: String,
    pub version_size: String,
    pub error_message: Option<String>,
}

impl FailedAssetEntry {
    pub(crate) fn from_record(r: &AssetRecord) -> Self {
        Self {
            id: r.id.clone(),
            version_size: r.version_size.as_str().to_string(),
            error_message: r.last_error.clone(),
        }
    }
}

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
    /// Up to `FAILED_ASSETS_CAP` structured failure entries (status='failed'
    /// in the state DB at report time). Empty on clean runs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed_assets: Vec<FailedAssetEntry>,
    /// Number of additional failure rows beyond `failed_assets.len()` that
    /// were omitted from the report. 0 when all failures fit under the cap.
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub failed_assets_truncated: usize,
}

const fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

/// User-facing options captured from the resolved Config. No secrets.
///
/// `size`, `live_photo_mode`, `live_photo_size`, `file_match_policy`, and
/// `library` are serialized as lowercased `{:?}` of the underlying enum
/// (e.g. `VersionSize::Original` → `"original"`). Those enum variant names
/// are therefore part of the `sync_report.json` wire format — renaming a
/// variant will silently change the emitted JSON. When a variant rename
/// is needed, either keep the old lowercase string here explicitly or
/// bump the report schema version.
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
    pub set_exif_rating: bool,
    pub set_exif_gps: bool,
    pub set_exif_description: bool,
    pub embed_xmp: bool,
    pub xmp_sidecar: bool,
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
            set_exif_rating: config.set_exif_rating,
            set_exif_gps: config.set_exif_gps,
            set_exif_description: config.set_exif_description,
            embed_xmp: config.embed_xmp,
            xmp_sidecar: config.xmp_sidecar,
            threads_num: config.threads_num,
            no_incremental: config.no_incremental,
            dry_run: config.dry_run,
        }
    }
}

/// Derive the `status` field for `sync_report.json` from the cycle outcome.
///
/// Zero-asset sync (nothing enumerated remotely, `failed_count == 0`) resolves
/// to `"success"` so operator automation sees exit-0 / status-success when a
/// library legitimately has no matching assets. `session_expired` dominates
/// `failed_count` because session loss explains any per-asset failures and
/// the correct caller action is re-authenticate, not retry.
pub(crate) fn sync_status_str(session_expired: bool, failed_count: usize) -> &'static str {
    if session_expired {
        "session_expired"
    } else if failed_count > 0 {
        "partial_failure"
    } else {
        "success"
    }
}

/// Write a JSON report to the given path atomically (temp file + rename).
pub(crate) fn write_report(path: &Path, report: &SyncReport) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(report)?;

    // Write to a temp file in the same directory, then rename for atomicity.
    let parent = path.parent().unwrap_or(Path::new("."));
    let temp_path = parent.join(format!(".kei-report-{}.tmp", std::process::id()));

    std::fs::write(&temp_path, json.as_bytes())?;
    atomic_install(&temp_path, path)?;

    tracing::debug!(path = %path.display(), "Wrote JSON report");
    Ok(())
}

/// Install `src` at `dst` atomically. Prefers `rename` (truly atomic on the
/// same device). On `EXDEV` (tmp and dst on different devices — rare but
/// possible when `path`'s parent is a bind mount / symlink to another fs),
/// copies `src` to a sibling of `dst` on the destination device, then
/// renames the sibling into place. A crash during the cross-device copy
/// leaves the sidecar tmp but never exposes a half-written `dst` to
/// consumers — which is the entire point of the atomic-write invariant.
fn atomic_install(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Err(rename_err) = std::fs::rename(src, dst) {
        let ext = dst.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
        let dst_sibling = dst.with_extension(format!("{ext}.kei-xdev-tmp-{}", std::process::id()));
        if let Err(copy_err) = std::fs::copy(src, &dst_sibling) {
            let _ = std::fs::remove_file(src);
            tracing::warn!(
                src = %src.display(),
                dst = %dst.display(),
                rename_err = %rename_err,
                copy_err = %copy_err,
                "rename failed and cross-device copy also failed"
            );
            return Err(rename_err);
        }
        if let Err(final_err) = std::fs::rename(&dst_sibling, dst) {
            let _ = std::fs::remove_file(&dst_sibling);
            let _ = std::fs::remove_file(src);
            return Err(final_err);
        }
        let _ = std::fs::remove_file(src);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::SkipBreakdown;

    #[test]
    fn sync_status_zero_assets_no_failures_is_success() {
        assert_eq!(sync_status_str(false, 0), "success");
    }

    #[test]
    fn sync_status_any_failure_is_partial_failure() {
        assert_eq!(sync_status_str(false, 1), "partial_failure");
        assert_eq!(sync_status_str(false, 999), "partial_failure");
    }

    #[test]
    fn sync_status_session_expired_dominates_failure_count() {
        assert_eq!(
            sync_status_str(true, 0),
            "session_expired",
            "session expiration with no per-asset failures is still session_expired"
        );
        assert_eq!(
            sync_status_str(true, 42),
            "session_expired",
            "session expiration dominates failed_count because the failures are attributable to session loss, not per-asset errors"
        );
    }

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
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
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
            failed_assets: vec![],
            failed_assets_truncated: 0,
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
    fn failed_assets_are_omitted_when_empty() {
        // serde(skip_serializing_if = "Vec::is_empty") on failed_assets
        // and is_zero_usize on failed_assets_truncated must keep clean-run
        // reports free of both fields.
        let report = SyncReport {
            version: "1",
            kei_version: "test",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "success".to_string(),
            options: RunOptions {
                username: "u".to_string(),
                directory: PathBuf::from("/x"),
                folder_structure: String::new(),
                size: String::new(),
                live_photo_mode: String::new(),
                live_photo_size: String::new(),
                file_match_policy: String::new(),
                albums: vec![],
                library: String::new(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads_num: 1,
                no_incremental: false,
                dry_run: false,
            },
            stats: SyncStats::default(),
            failed_assets: vec![],
            failed_assets_truncated: 0,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            !json.contains("failed_assets"),
            "empty failed_assets should not appear in JSON: {json}"
        );
        assert!(
            !json.contains("failed_assets_truncated"),
            "zero truncated counter should not appear: {json}"
        );
    }

    #[test]
    fn failed_assets_serialize_when_present() {
        let entry = FailedAssetEntry {
            id: "ASSET_1".to_string(),
            version_size: "original".to_string(),
            error_message: Some("HTTP 429".to_string()),
        };
        let report = SyncReport {
            version: "1",
            kei_version: "test",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "partial_failure".to_string(),
            options: RunOptions {
                username: "u".to_string(),
                directory: PathBuf::from("/x"),
                folder_structure: String::new(),
                size: String::new(),
                live_photo_mode: String::new(),
                live_photo_size: String::new(),
                file_match_policy: String::new(),
                albums: vec![],
                library: String::new(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads_num: 1,
                no_incremental: false,
                dry_run: false,
            },
            stats: SyncStats {
                failed: 1,
                ..SyncStats::default()
            },
            failed_assets: vec![entry],
            failed_assets_truncated: 0,
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(parsed["failed_assets"][0]["id"], "ASSET_1");
        assert_eq!(parsed["failed_assets"][0]["version_size"], "original");
        assert_eq!(parsed["failed_assets"][0]["error_message"], "HTTP 429");
        assert!(parsed["failed_assets_truncated"].is_null());
    }

    #[test]
    fn failed_assets_truncated_emitted_when_nonzero() {
        let report = SyncReport {
            version: "1",
            kei_version: "test",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "partial_failure".to_string(),
            options: RunOptions {
                username: "u".to_string(),
                directory: PathBuf::from("/x"),
                folder_structure: String::new(),
                size: String::new(),
                live_photo_mode: String::new(),
                live_photo_size: String::new(),
                file_match_policy: String::new(),
                albums: vec![],
                library: String::new(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads_num: 1,
                no_incremental: false,
                dry_run: false,
            },
            stats: SyncStats::default(),
            failed_assets: vec![FailedAssetEntry {
                id: "x".to_string(),
                version_size: "original".to_string(),
                error_message: None,
            }],
            failed_assets_truncated: 847,
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(parsed["failed_assets_truncated"], 847);
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
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads_num: 3,
                no_incremental: false,
                dry_run: false,
            },
            stats: SyncStats::default(),
            failed_assets: vec![],
            failed_assets_truncated: 0,
        };

        write_report(&path, &report).expect("write_report");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["version"], "1");
        assert_eq!(parsed["options"]["username"], "test@example.com");
    }

    /// Happy-path test of the atomic_install helper. Same-device rename
    /// succeeds on the first attempt; the sidecar tmp is never created.
    #[test]
    fn atomic_install_same_device_rename_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.json");
        std::fs::write(&src, b"hello").unwrap();

        atomic_install(&src, &dst).expect("atomic_install");

        assert!(!src.exists(), "src must be consumed by the rename");
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");

        // No cross-device sidecar left behind.
        for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains("kei-xdev-tmp"),
                "unexpected sidecar tmp {name}",
            );
        }
    }

    /// If the source does not exist, atomic_install returns the rename error
    /// and does not poison `dst` with a partially-written file.
    #[test]
    fn atomic_install_missing_src_returns_err_without_touching_dst() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("nope.tmp");
        let dst = dir.path().join("dst.json");

        assert!(atomic_install(&src, &dst).is_err());
        assert!(!dst.exists(), "dst must not be created on failure");
    }
}
