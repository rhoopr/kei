//! JSON sync report generation.
//!
//! Writes a structured JSON summary after each sync cycle for machine consumption
//! (monitoring tools, Home Assistant, webhooks). In watch mode the file is
//! overwritten every cycle — it's the current cycle's state, not a history log.
//!
//! # Schema v3
//!
//! v3 adds metadata-capture revision and repair progress to `stats`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::download::SyncStats;
use crate::fs_util::atomic_install;
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
            id: r.id.to_string(),
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
    /// Sync outcome: "success", "interrupted", "partial_failure", or "session_expired".
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
/// `resolution`, `live_photo_mode`, `live_resolution`, `file_match_policy`, and
/// `library` are serialized as lowercased `{:?}` of the underlying enum
/// (e.g. `VersionSize::Original` → `"original"`). Those enum variant names
/// are therefore part of the `sync_report.json` wire format — renaming a
/// variant will silently change the emitted JSON. When a variant rename
/// is needed, either keep the old lowercase string here explicitly or
/// bump the report schema version.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunOptions {
    pub username: String,
    pub download_dir: PathBuf,
    pub folder_structure: String,
    pub resolution: String,
    pub live_photo_mode: String,
    pub live_resolution: String,
    pub edited: bool,
    #[serde(skip_serializing_if = "edited_naming_is_default")]
    pub edited_naming: crate::types::EditedNaming,
    pub alternative: bool,
    pub raw_policy: String,
    pub file_match_policy: String,
    pub force_resolution: bool,
    pub albums: Vec<String>,
    pub library: String,
    pub media: Vec<crate::config::MediaKind>,
    pub skip_videos: bool,
    pub skip_photos: bool,
    pub set_exif_datetime: bool,
    pub set_exif_rating: bool,
    pub set_exif_gps: bool,
    pub set_exif_description: bool,
    pub embed_xmp: bool,
    pub xmp_sidecar: bool,
    pub threads: u16,
    pub dry_run: bool,
}

impl RunOptions {
    /// Build from the resolved Config. Only includes user-facing settings.
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        Self {
            username: config.auth.username.clone(),
            download_dir: config.download.directory.clone(),
            folder_structure: config.download.folder_structure.clone(),
            resolution: format!("{:?}", config.photos.resolution).to_lowercase(),
            live_photo_mode: format!("{:?}", config.photos.live_photo_mode).to_lowercase(),
            live_resolution: format!("{:?}", config.photos.live_resolution).to_lowercase(),
            edited: config.photos.edited,
            edited_naming: config.photos.edited_naming,
            alternative: config.photos.alternative,
            raw_policy: match config.photos.raw_policy {
                crate::types::RawPolicy::AsIs => "as-is".to_string(),
                crate::types::RawPolicy::PreferRaw => "prefer-raw".to_string(),
                crate::types::RawPolicy::PreferJpeg => "prefer-jpeg".to_string(),
            },
            file_match_policy: format!("{:?}", config.photos.file_match_policy).to_lowercase(),
            force_resolution: config.photos.force_resolution,
            albums: config.filters.selection.albums.to_raw(),
            library: config.filters.selection.libraries.to_raw().join(","),
            media: config.filters.media.to_kinds(),
            skip_videos: config.filters.skip_videos,
            skip_photos: config.filters.skip_photos,
            set_exif_datetime: config.metadata.set_exif_datetime,
            set_exif_rating: config.metadata.set_exif_rating,
            set_exif_gps: config.metadata.set_exif_gps,
            set_exif_description: config.metadata.set_exif_description,
            #[cfg(feature = "xmp")]
            embed_xmp: config.metadata.embed_xmp,
            #[cfg(not(feature = "xmp"))]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: config.metadata.xmp_sidecar,
            #[cfg(not(feature = "xmp"))]
            xmp_sidecar: false,
            threads: config.download.threads_num,
            dry_run: config.runtime.dry_run,
        }
    }
}

/// Write a JSON report to the given path atomically.
///
/// Serialization runs synchronously (small — typically a few KB), but the
/// filesystem write + rename are pushed to the blocking pool so this can
/// be called from async contexts without stalling a tokio worker.
pub(crate) async fn write_report(path: &Path, report: &SyncReport) -> anyhow::Result<()> {
    use anyhow::Context;
    let json = serde_json::to_string_pretty(report)?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let parent = path.parent().unwrap_or(Path::new("."));
        let temp_path = parent.join(format!(".kei-report-{}.tmp", std::process::id()));

        std::fs::write(&temp_path, json.as_bytes())
            .with_context(|| format!("Could not write report to {}", temp_path.display()))?;
        atomic_install(&temp_path, &path)
            .with_context(|| format!("Could not install report at {}", path.display()))?;

        tracing::debug!(path = %path.display(), "Wrote JSON report");
        Ok(())
    })
    .await?
}

fn edited_naming_is_default(value: &crate::types::EditedNaming) -> bool {
    *value == crate::types::EditedNaming::Suffix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::SkipBreakdown;

    #[test]
    fn primary_layout_report_inventory_is_distinct_from_network_counters() {
        let mut stats = crate::download::SyncStats::default();
        let json = serde_json::to_value(&stats).unwrap();
        assert!(json.get("primary_layout").is_none());
        stats.primary_layout = Some(crate::state::types::PrimaryLayoutSummary {
            bound_families: 5,
            preserved_files: 12,
            pending_operations: 2,
            held_operations: 1,
        });
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["primary_layout"]["preserved_files"], 12);
        assert_eq!(json["primary_layout"]["held_operations"], 1);
        assert_eq!(json["downloaded"], 0);
        assert_eq!(json["bytes_downloaded"], 0);
    }

    #[test]
    fn report_serialization_roundtrip() {
        let report = SyncReport {
            version: "3",
            kei_version: "0.7.12",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "success".to_string(),
            options: RunOptions {
                username: "user@example.com".to_string(),
                download_dir: PathBuf::from("/photos"),
                folder_structure: "{:%Y/%m/%d}".to_string(),
                resolution: "original".to_string(),
                live_photo_mode: "original".to_string(),
                live_resolution: "original".to_string(),
                edited: false,
                edited_naming: crate::types::EditedNaming::Suffix,
                alternative: false,
                raw_policy: "as-is".to_string(),
                force_resolution: false,
                file_match_policy: "name-size-dedup".to_string(),
                albums: vec!["Favorites".to_string()],
                library: "personal".to_string(),
                media: crate::config::MediaSelection::all().to_kinds(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: true,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads: 4,
                dry_run: false,
            },
            stats: SyncStats {
                assets_seen: 400,
                api_total_at_start: Some(405),
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
                metadata_capture_revision: Some(1),
                metadata_capture_refreshed: 25,
                metadata_capture_failures: 2,
                metadata_capture_remaining: 75,
                metadata_capture_unresolved: 19,
                metadata_capture_deferred: 18,
                inventory_drop_warnings: 1,
                inventory_drop_assets: 5,
                inventory_drop_percent: Some(1.2),
                inventory_drop_previous_total: Some(410),
                inventory_drop_current_total: Some(405),
                inventory_drop_library: Some("PrimarySync".to_string()),
                elapsed_secs: 263.5,
                ..SyncStats::default()
            },
            failed_assets: vec![],
            failed_assets_truncated: 0,
        };

        let json = serde_json::to_string_pretty(&report).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

        assert_eq!(parsed["version"], "3");
        assert_eq!(parsed["status"], "success");
        assert_eq!(parsed["stats"]["downloaded"], 50);
        assert_eq!(parsed["stats"]["api_total_at_start"], 405);
        assert_eq!(parsed["stats"]["metadata_capture_revision"], 1);
        assert_eq!(parsed["stats"]["metadata_capture_refreshed"], 25);
        assert_eq!(parsed["stats"]["metadata_capture_failures"], 2);
        assert_eq!(parsed["stats"]["metadata_capture_remaining"], 75);
        assert_eq!(parsed["stats"]["metadata_capture_unresolved"], 19);
        assert_eq!(parsed["stats"]["metadata_capture_deferred"], 18);
        assert_eq!(parsed["stats"]["inventory_drop_warnings"], 1);
        assert_eq!(parsed["stats"]["inventory_drop_previous_total"], 410);
        assert_eq!(parsed["stats"]["inventory_drop_current_total"], 405);
        assert_eq!(parsed["stats"]["inventory_drop_library"], "PrimarySync");
        assert_eq!(parsed["stats"]["skipped"]["by_state"], 300);
        assert_eq!(parsed["options"]["username"], "user@example.com");
        assert!(
            parsed["options"]["set_exif_datetime"]
                .as_bool()
                .unwrap_or(false)
        );
    }

    #[test]
    fn run_options_from_config_uses_schema_v2_option_names() {
        let download_dir = tempfile::tempdir().expect("download dir");
        let globals = crate::config::GlobalArgs {
            username: Some("report@example.com".to_string()),
            domain: None,
            data_dir: None,
        };
        let sync = crate::cli::SyncArgs {
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(download_dir.path().display().to_string()),
                threads: Some(6),
                ..Default::default()
            },
            ..crate::cli::SyncArgs::default()
        };
        let config = crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            sync,
            None,
        )
        .expect("build config");

        let options = RunOptions::from_config(&config);
        assert_eq!(options.download_dir, download_dir.path());
        assert_eq!(options.threads, 6);

        let json = serde_json::to_value(&options).expect("serialize options");
        assert_eq!(
            json["download_dir"],
            download_dir.path().display().to_string()
        );
        assert_eq!(json["threads"], 6);
        assert!(
            json.get("directory").is_none(),
            "schema v3 must not emit the legacy options.directory key"
        );
        assert!(
            json.get("threads_num").is_none(),
            "schema v3 must not emit the legacy options.threads_num key"
        );
    }

    #[test]
    fn run_options_from_config_reports_media_selection() {
        let download_dir = tempfile::tempdir().expect("download dir");
        let globals = crate::config::GlobalArgs {
            username: Some("report@example.com".to_string()),
            domain: None,
            data_dir: None,
        };
        let sync = crate::cli::SyncArgs {
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(download_dir.path().display().to_string()),
                ..Default::default()
            },
            ..crate::cli::SyncArgs::default()
        };
        let toml: crate::config::TomlConfig =
            toml::from_str("[filters]\nmedia = [\"photos\", \"videos\"]\n")
                .expect("parse media config");
        let config = crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            sync,
            Some(&toml),
        )
        .expect("build config");

        let options = RunOptions::from_config(&config);

        assert_eq!(
            options.media,
            vec![
                crate::config::MediaKind::Photos,
                crate::config::MediaKind::Videos,
            ],
        );
        assert!(!options.skip_videos);
        assert!(!options.skip_photos);

        let json = serde_json::to_value(&options).expect("serialize options");
        assert_eq!(json["media"], serde_json::json!(["photos", "videos"]));
    }

    #[test]
    fn failed_assets_are_omitted_when_empty() {
        // serde(skip_serializing_if = "Vec::is_empty") on failed_assets
        // and is_zero_usize on failed_assets_truncated must keep clean-run
        // reports free of both fields.
        let report = SyncReport {
            version: "3",
            kei_version: "test",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "success".to_string(),
            options: RunOptions {
                username: "u".to_string(),
                download_dir: PathBuf::from("/x"),
                folder_structure: String::new(),
                resolution: String::new(),
                live_photo_mode: String::new(),
                live_resolution: String::new(),
                edited: false,
                edited_naming: crate::types::EditedNaming::Suffix,
                alternative: false,
                raw_policy: String::new(),
                force_resolution: false,
                file_match_policy: String::new(),
                albums: vec![],
                library: String::new(),
                media: crate::config::MediaSelection::all().to_kinds(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads: 1,
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
            version: "3",
            kei_version: "test",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "partial_failure".to_string(),
            options: RunOptions {
                username: "u".to_string(),
                download_dir: PathBuf::from("/x"),
                folder_structure: String::new(),
                resolution: String::new(),
                live_photo_mode: String::new(),
                live_resolution: String::new(),
                edited: false,
                edited_naming: crate::types::EditedNaming::Suffix,
                alternative: false,
                raw_policy: String::new(),
                force_resolution: false,
                file_match_policy: String::new(),
                albums: vec![],
                library: String::new(),
                media: crate::config::MediaSelection::all().to_kinds(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads: 1,
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
            version: "3",
            kei_version: "test",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "partial_failure".to_string(),
            options: RunOptions {
                username: "u".to_string(),
                download_dir: PathBuf::from("/x"),
                folder_structure: String::new(),
                resolution: String::new(),
                live_photo_mode: String::new(),
                live_resolution: String::new(),
                edited: false,
                edited_naming: crate::types::EditedNaming::Suffix,
                alternative: false,
                raw_policy: String::new(),
                force_resolution: false,
                file_match_policy: String::new(),
                albums: vec![],
                library: String::new(),
                media: crate::config::MediaSelection::all().to_kinds(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads: 1,
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

    #[tokio::test]
    async fn write_report_creates_valid_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");

        let report = SyncReport {
            version: "3",
            kei_version: "0.7.12",
            timestamp: "2026-04-15T12:00:00Z".to_string(),
            status: "success".to_string(),
            options: RunOptions {
                username: "test@example.com".to_string(),
                download_dir: PathBuf::from("/tmp/photos"),
                folder_structure: "{:%Y/%m/%d}".to_string(),
                resolution: "original".to_string(),
                live_photo_mode: "original".to_string(),
                live_resolution: "original".to_string(),
                edited: false,
                edited_naming: crate::types::EditedNaming::Suffix,
                alternative: false,
                raw_policy: "as-is".to_string(),
                force_resolution: false,
                file_match_policy: "name-size-dedup".to_string(),
                albums: vec![],
                library: "personal".to_string(),
                media: crate::config::MediaSelection::all().to_kinds(),
                skip_videos: false,
                skip_photos: false,
                set_exif_datetime: false,
                set_exif_rating: false,
                set_exif_gps: false,
                set_exif_description: false,
                embed_xmp: false,
                xmp_sidecar: false,
                threads: 3,
                dry_run: false,
            },
            stats: SyncStats::default(),
            failed_assets: vec![],
            failed_assets_truncated: 0,
        };

        write_report(&path, &report).await.expect("write_report");

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["version"], "3");
        assert_eq!(parsed["options"]["username"], "test@example.com");
    }
}
