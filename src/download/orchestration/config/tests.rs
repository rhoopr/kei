use std::sync::Arc;

use chrono::{DateTime, Utc};
use tempfile::TempDir;

use crate::types::{AssetVersionSize, FileMatchPolicy, LivePhotoMode, RawPolicy};

use super::super::test_support::test_config;
use super::{
    DownloadConfig, compute_config_hash, hash_download_config, hash_legacy_download_config,
};

#[test]
fn test_hash_download_config_deterministic() {
    let config = test_config();
    let hash1 = hash_download_config(&config);
    let hash2 = hash_download_config(&config);
    assert_eq!(hash1, hash2);
    assert_eq!(hash1.len(), 16); // 8 bytes hex-encoded
}

#[test]
fn test_hash_download_config_changes_on_directory() {
    let mut config1 = test_config();
    config1.directory = std::sync::Arc::from(std::path::Path::new("/photos/a"));
    let mut config2 = test_config();
    config2.directory = std::sync::Arc::from(std::path::Path::new("/photos/b"));
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_folder_structure() {
    let mut config1 = test_config();
    config1.folder_structure = "{:%Y/%m/%d}".to_string();
    let mut config2 = test_config();
    config2.folder_structure = "{:%Y/%m}".to_string();
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_file_match_policy() {
    let mut config1 = test_config();
    config1.file_match_policy = FileMatchPolicy::NameSizeDedupWithSuffix;
    let mut config2 = test_config();
    config2.file_match_policy = FileMatchPolicy::NameId7;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_keep_unicode() {
    let mut config1 = test_config();
    config1.keep_unicode_in_filenames = false;
    let mut config2 = test_config();
    config2.keep_unicode_in_filenames = true;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_ignores_unrelated_fields() {
    let mut config1 = test_config();
    config1.concurrent_downloads = 1;
    let mut config2 = test_config();
    config2.concurrent_downloads = 16;
    // These fields don't affect download paths, so hash should be the same
    assert_eq!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

// ── NormalizedPath additional tests ──────────────────────────────────

// ── hash_download_config additional sensitivity ─────────────────────

#[test]
fn test_hash_download_config_changes_on_resolution() {
    let mut config1 = test_config();
    config1.resolution = crate::types::PhotoResolution::Original;
    let mut config2 = test_config();
    config2.resolution = crate::types::PhotoResolution::Medium;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_live_resolution() {
    let mut config1 = test_config();
    config1.live_resolution = AssetVersionSize::LiveOriginal;
    let mut config2 = test_config();
    config2.live_resolution = AssetVersionSize::LiveMedium;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_live_photo_mov_filename_policy() {
    let mut config1 = test_config();
    config1.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Suffix;
    let mut config2 = test_config();
    config2.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_raw_policy() {
    let mut config1 = test_config();
    config1.raw_policy = RawPolicy::AsIs;
    let mut config2 = test_config();
    config2.raw_policy = RawPolicy::PreferRaw;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_ignores_skip_created_before() {
    let mut config1 = test_config();
    config1.skip_created_before = None;
    let mut config2 = test_config();
    config2.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    ));
    assert_eq!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_ignores_skip_created_after() {
    let mut config1 = test_config();
    config1.skip_created_after = None;
    let mut config2 = test_config();
    config2.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    ));
    assert_eq!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_ignores_created_date_semantics() {
    let date = chrono::NaiveDate::from_ymd_opt(2025, 2, 1).unwrap();
    let mut instant = test_config();
    instant.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        date.and_time(chrono::NaiveTime::MIN).and_utc(),
    ));
    let mut capture_date = instant.clone();
    capture_date.skip_created_before = Some(crate::config::CreatedDateFilter::CaptureDate(date));

    assert_eq!(
        hash_download_config(&instant),
        hash_download_config(&capture_date)
    );
}

#[test]
fn test_legacy_download_config_hash_changes_on_skip_created_before() {
    let mut config1 = test_config();
    config1.skip_created_before = None;
    let mut config2 = test_config();
    config2.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    ));
    assert_ne!(
        hash_legacy_download_config(&config1),
        hash_legacy_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_recent() {
    let mut config1 = test_config();
    config1.recent = None;
    let mut config2 = test_config();
    config2.recent = Some(100);
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_recent_scope_when_recent_is_set() {
    let mut config1 = test_config();
    config1.recent = Some(100);
    config1.recent_scope = crate::cli::RecentScope::Global;
    let mut config2 = test_config();
    config2.recent = Some(100);
    config2.recent_scope = crate::cli::RecentScope::PerFilter;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_force_resolution() {
    let mut config1 = test_config();
    config1.force_resolution = false;
    let mut config2 = test_config();
    config2.force_resolution = true;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_media_videos() {
    let mut config1 = test_config();
    config1.media.videos = true;
    let mut config2 = test_config();
    config2.media.videos = false;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_changes_on_media_photos() {
    let mut config1 = test_config();
    config1.media.photos = true;
    let mut config2 = test_config();
    config2.media.photos = false;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_download_config_is_16_hex_chars() {
    let config = test_config();
    let hash = hash_download_config(&config);
    assert_eq!(hash.len(), 16);
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "Hash should be hex chars only, got: {hash}"
    );
}

// ── compute_config_hash equivalence ────────────────────────────────

/// `compute_config_hash` tracks CloudKit token safety, not local path
/// trust-state. Verify it produces a valid hex hash and is deterministic.
#[test]
fn test_compute_config_hash_matches_hash_download_config() {
    use crate::config::Config;

    let dl_config = test_config();
    let globals = crate::config::GlobalArgs {
        username: Some("u@example.com".to_string()),
        domain: None,
        data_dir: Some("/tmp".to_string()),
    };
    let app_config = Config::build(
        &globals,
        &crate::cli::PasswordArgs::default(),
        crate::cli::SyncArgs {
            recent: dl_config.recent.map(crate::cli::RecentLimit::Count),
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(dl_config.directory.display().to_string()),
                folder_structure: Some(dl_config.folder_structure.clone()),
                resolution: Some(crate::types::PhotoResolution::Original),
                ..Default::default()
            },
            no_progress_bar: true,
            ..Default::default()
        },
        None,
    )
    .unwrap();

    // compute_config_hash tracks only CloudKit-token safety fields. Verify
    // it is deterministic and valid hex.
    let hash1 = compute_config_hash(&app_config);
    let hash2 = compute_config_hash(&app_config);
    assert_eq!(hash1, hash2, "compute_config_hash must be deterministic");
    assert_eq!(hash1.len(), 16);
    assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()));

    // Album changes are handled by membership snapshots and targeted
    // backfill, not by invalidating the zone token.
    let mut config_with_album = app_config;
    config_with_album.filters.selection.albums =
        crate::selection::parse_album_selector(&["Favorites".to_string()], true).unwrap();
    let hash3 = compute_config_hash(&config_with_album);
    assert_eq!(
        hash1, hash3,
        "adding an album must keep the zone-token hash"
    );
}

// ── Gap coverage: retry_only known_ids filtering ────────────────────

// ── Gap coverage: skip_created_before AND skip_created_after ────────

// ── Gap coverage: incremental Modified events are downloadable ──────

// ── Gap coverage: NameId7 produces task when file at original path ──

// ── compute_config_hash tests ──────────────────────────────────

/// Build a `Config` via `Config::build` with the given overrides.
/// Uses a tempdir for cookie_directory so tests don't touch the real filesystem.
fn build_config_with(
    cookie_dir: &std::path::Path,
    directory: &str,
    overrides: impl FnOnce(&mut crate::cli::SyncArgs),
) -> crate::config::Config {
    use crate::cli::SyncArgs;
    use crate::config::GlobalArgs;

    let globals = GlobalArgs {
        username: Some("test@example.com".to_string()),
        domain: None,
        data_dir: Some(cookie_dir.to_string_lossy().into_owned()),
    };
    let mut sync = SyncArgs {
        config_overrides: crate::config::SyncConfigOverrides {
            download_dir: Some(directory.to_string()),
            ..Default::default()
        },
        ..SyncArgs::default()
    };
    overrides(&mut sync);
    crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
        .expect("Config::build should succeed")
}

#[test]
fn test_compute_config_hash_same_config_same_hash() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |_| {});
    assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
}

#[test]
fn test_compute_config_hash_different_directory() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos/a", |_| {});
    let b = build_config_with(tmp.path(), "/photos/b", |_| {});
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "download directory is path-only and must not invalidate the CloudKit zone token"
    );
}

#[test]
fn test_compute_config_hash_different_size() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.resolution = Some(crate::types::PhotoResolution::Medium);
    });
    assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
}

#[test]
fn test_compute_config_hash_different_skip_videos() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.skip_videos = Some(true);
    });
    assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
}

#[test]
fn test_compute_config_hash_different_albums() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.albums = vec!["Favorites".to_string()];
    });
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "album selection changes are handled by membership snapshots and targeted backfill"
    );
}

#[test]
fn test_compute_config_hash_different_inline_album_excludes() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.albums = vec!["!Hidden".to_string()];
    });
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "removing albums should not invalidate the CloudKit zone token"
    );
}

#[test]
fn test_compute_config_hash_different_live_photo_mode() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.live_photo_mode = Some(LivePhotoMode::Skip);
    });
    assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
}

#[test]
fn test_compute_config_hash_different_smart_folders() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.smart_folders = vec!["Favorites".to_string()];
    });
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "smart-folder selection changes are handled by targeted refresh"
    );
}

#[test]
fn test_compute_config_hash_different_unfiled() {
    // Same silent-miss vector for the unfiled selector: --unfiled true
    // (default) → false changes whether the unfiled pass runs at all.
    // A regression that omits this from the hash leaves a stale token
    // pointing past assets the previous cycle would have caught.
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.unfiled = Some(false);
    });
    assert_ne!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "changing --unfiled must change the config hash so the \
         stored sync token is invalidated"
    );
}

#[test]
fn test_compute_config_hash_different_library() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.libraries = vec!["all".to_string()];
    });
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "library selection changes should hydrate only newly selected libraries"
    );
}

#[test]
fn test_compute_config_hash_path_only_changes_same_hash() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos-a", |_| {});
    let b = build_config_with(tmp.path(), "/photos-b", |s| {
        s.config_overrides.folder_structure = Some("%Y/%m".to_string());
        s.config_overrides.folder_structure_albums = Some("{album}/albums/%Y".to_string());
        s.config_overrides.folder_structure_smart_folders = Some("{smart-folder}/%Y".to_string());
        s.config_overrides.keep_unicode_in_filenames = Some(true);
    });
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "path-only changes must not invalidate the CloudKit zone token"
    );
}

#[test]
fn test_compute_config_hash_different_recent_same_hash() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.recent = Some(crate::cli::RecentLimit::Count(100));
    });
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "recent is intentionally excluded from the config hash"
    );
}

#[test]
fn test_compute_config_hash_different_dry_run_same_hash() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.dry_run = true;
    });
    assert_eq!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "dry_run is a per-run flag and should not affect the config hash"
    );
}

// ── filter_asset_to_tasks edge-case tests ──────────────────────

// ── LivePhotoMode + filename_exclude filter tests ─────────────

// ── exclude_asset_ids filter tests ─────────────────────────────

#[test]
fn test_hash_changes_on_live_photo_mode() {
    let config1 = test_config();
    let mut config2 = test_config();
    config2.live_photo_mode = LivePhotoMode::Skip;
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_changes_on_filename_exclude() {
    let config1 = test_config();
    let mut config2 = test_config();
    config2.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

// ── requires_per_pass_paths predicate ──────────────────────────

fn config_with_templates(base: &str, albums: &str, smart_folders: &str) -> DownloadConfig {
    let mut c = test_config();
    c.folder_structure = base.to_string();
    c.folder_structure_albums = Arc::from(albums);
    c.folder_structure_smart_folders = Arc::from(smart_folders);
    c
}

#[test]
fn requires_per_pass_paths_fires_on_v013_defaults() {
    // v0.13 defaults carry per-pass tokens in the per-category fields.
    // Returning false here was the regression that silently routed every
    // album-pass photo through the unfiled template.
    assert!(test_config().requires_per_pass_paths());
}

#[test]
fn requires_per_pass_paths_fires_on_legacy_album_in_base() {
    assert!(
        config_with_templates("{album}/%Y", "{album}/%Y", "{album}/%Y").requires_per_pass_paths()
    );
}

#[test]
fn requires_per_pass_paths_fires_on_smart_folder_token() {
    assert!(
        config_with_templates("%Y/%m/%d", "%Y/%m/%d", "{smart-folder}").requires_per_pass_paths()
    );
}

#[test]
fn requires_per_pass_paths_fires_on_library_token() {
    assert!(
        config_with_templates("{library}/%Y", "{library}/%Y", "{library}/%Y")
            .requires_per_pass_paths()
    );
}

#[test]
fn requires_per_pass_paths_fires_on_per_category_template_diverging_from_base() {
    assert!(
        config_with_templates("%Y/%m/%d", "MyAlbums/%Y/%m", "%Y/%m/%d").requires_per_pass_paths()
    );
}

#[test]
fn requires_per_pass_paths_false_when_all_templates_are_identical_literals() {
    // Pure-literal, identical across all three fields, no per-pass token:
    // the merged-stream branch is safe.
    assert!(!config_with_templates("%Y/%m/%d", "%Y/%m/%d", "%Y/%m/%d").requires_per_pass_paths());
}

// ── with_pass per-kind template selection ─────────────────────

fn make_pass(kind: crate::commands::PassKind, name: &str) -> crate::commands::AlbumPass {
    use crate::icloud::photos::PhotoAlbum;
    crate::commands::AlbumPass {
        kind,
        album: PhotoAlbum::stub_for_test(Arc::from(name)),
        exclude_ids: std::sync::Arc::new(rustc_hash::FxHashSet::default()),
    }
}

#[test]
fn test_with_pass_album_uses_albums_template() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure_albums = Arc::from("{album}/%Y/%m/%d");
    let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
    assert_eq!(derived.folder_structure, "Vacation/%Y/%m/%d");
    assert_eq!(derived.album_name.as_deref(), Some("Vacation"));
}

#[test]
fn test_with_pass_smart_folder_uses_smart_folders_template() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
    let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
    assert_eq!(derived.folder_structure, "Favorites/%Y");
}

#[test]
fn test_with_pass_smart_folder_ignores_albums_template() {
    // Spec: smart-folder passes use folder_structure_smart_folders, not
    // folder_structure_albums. Using the wrong template would cause every
    // smart-folder pass to substitute the smart-folder name into a
    // user-customised album path (e.g. "My/Albums/{album}/..." would
    // mis-render as "My/Albums/Favorites/...").
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure_albums = Arc::from("{album}/album-tree");
    config.folder_structure_smart_folders = Arc::from("{smart-folder}/sf-tree");
    let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Videos"));
    assert!(derived.folder_structure.contains("sf-tree"));
    assert!(!derived.folder_structure.contains("album-tree"));
}

#[test]
fn test_with_pass_unfiled_uses_base_folder_structure() {
    // Unfiled pass keeps the legacy `{album}` token in `folder_structure`
    // so existing configs with `--folder-structure "{album}/..."` still
    // produce the same on-disk tree.
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure = "%Y/%m/%d".to_string();
    let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
    assert_eq!(derived.folder_structure, "%Y/%m/%d");
}

#[test]
fn test_with_pass_unfiled_collapses_album_token_to_empty() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure = "{album}/%Y/%m/%d".to_string();
    let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
    // Empty name strips the `{album}` segment for backwards compat.
    assert!(!derived.folder_structure.contains("{album}"));
}

#[test]
fn test_with_pass_album_sanitizes_name() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure_albums = Arc::from("{album}/%Y");
    let derived = config.with_pass(&make_pass(PassKind::Album, "My/Album"));
    // Path separators in album names must be sanitised before substitution.
    assert!(!derived.folder_structure.starts_with("My/Album"));
}

#[test]
fn test_with_pass_expands_library_token_with_truncation() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure_albums = Arc::from("{library}/{album}/%Y");
    config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
    let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
    assert_eq!(
        derived.folder_structure, "SharedSync-A1B2C3D4/Vacation/%Y",
        "shared-zone UUIDs must truncate to 8 chars in path output"
    );
}

#[test]
fn test_with_pass_library_token_passthrough_for_primary() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure = "{library}/%Y/%m/%d".to_string();
    // Default `library` is "PrimarySync" via `test_default`.
    let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
    assert_eq!(derived.folder_structure, "PrimarySync/%Y/%m/%d");
}

#[test]
fn test_with_pass_library_token_in_smart_folder_template() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure_smart_folders = Arc::from("{library}/{smart-folder}");
    config.library = Arc::from("SharedSync-DEADBEEF-aaaa-bbbb-cccc-dddddddddddd");
    let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
    assert_eq!(derived.folder_structure, "SharedSync-DEADBEEF/Favorites");
}

#[test]
fn test_with_pass_state_db_library_uses_full_zone_name() {
    // Path rendering truncates the zone for readability, but the
    // state-DB key (DownloadConfig.library) keeps the full zone name
    // verbatim so two zones whose 8-char prefixes happen to collide
    // still get distinct PKs in the assets table.
    use crate::commands::PassKind;
    let mut config = test_config();
    config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
    let derived = config.with_pass(&make_pass(PassKind::Album, "Trip"));
    assert_eq!(
        &*derived.library,
        "SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890"
    );
}

#[test]
fn test_with_pass_preserves_all_fields() {
    use crate::commands::PassKind;
    let mut config = test_config();
    config.folder_structure_albums = Arc::from("{album}/%Y");
    config.media.photos = false;
    config.media.videos = false;
    config.live_photo_mode = LivePhotoMode::ImageOnly;
    config.force_resolution = true;
    config.keep_unicode_in_filenames = true;
    config.metadata.set_exif_datetime = true;
    config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
    config.temp_suffix = std::sync::Arc::from(".custom-tmp");
    let derived = config.with_pass(&make_pass(PassKind::Album, "Test"));
    assert!(!derived.media.photos);
    assert!(!derived.media.videos);
    assert_eq!(derived.live_photo_mode, LivePhotoMode::ImageOnly);
    assert!(derived.force_resolution);
    assert!(derived.keep_unicode_in_filenames);
    assert!(derived.metadata.set_exif_datetime);
    assert_eq!(derived.filename_exclude.len(), 1);
    assert_eq!(&*derived.temp_suffix, ".custom-tmp");
    assert_eq!(derived.directory, config.directory);
}

// ── extract_skip_candidates: filename_exclude ─────────────────

// ── compute_config_hash: filename_exclude ─────────────────────

#[test]
fn test_compute_config_hash_different_filename_exclude() {
    let tmp = TempDir::new().unwrap();
    let a = build_config_with(tmp.path(), "/photos", |_| {});
    let b = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.filename_exclude = vec!["*.AAE".to_string()];
    });
    assert_ne!(
        compute_config_hash(&a),
        compute_config_hash(&b),
        "changing filename_exclude should change the config hash"
    );
}

#[test]
fn test_hash_changes_on_folder_structure_albums() {
    // Per-category templates affect path resolution, so the trust-state
    // hash must change with them or stale records pin assets to the wrong
    // tree on the next run.
    let mut config1 = test_config();
    let mut config2 = test_config();
    config1.folder_structure_albums = Arc::from("{album}");
    config2.folder_structure_albums = Arc::from("{album}/%Y");
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

#[test]
fn test_hash_changes_on_folder_structure_smart_folders() {
    let mut config1 = test_config();
    let mut config2 = test_config();
    config1.folder_structure_smart_folders = Arc::from("{smart-folder}");
    config2.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
    assert_ne!(
        hash_download_config(&config1),
        hash_download_config(&config2)
    );
}

// ── Golden-hash stability tests ─────────────────────────────────
//
// These pin specific config values to specific hex outputs. If any
// test fails, it means the hash encoding changed -- which would
// trigger unnecessary full re-syncs for all users. Only update the
// expected values when the hash change is intentional.

#[test]
fn golden_hash_download_config_defaults() {
    let config = test_config();
    let hash = hash_download_config(&config);
    assert_eq!(
        hash, "20baba076dd97143",
        "hash_download_config golden hash changed -- this will trigger full re-syncs"
    );
    assert_eq!(
        hash_legacy_download_config(&config),
        "c3f2be1a9e394951",
        "legacy hash changed -- matching stored hashes would no longer migrate safely"
    );
}

#[test]
fn golden_hash_download_config_non_defaults() {
    let mut config = test_config();
    config.directory = std::sync::Arc::from(std::path::Path::new("/my/photos"));
    config.folder_structure = "{:%Y/%m}".to_string();
    config.resolution = crate::types::PhotoResolution::Medium;
    config.live_resolution = AssetVersionSize::LiveMedium;
    config.file_match_policy = FileMatchPolicy::NameId7;
    config.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
    config.raw_policy = RawPolicy::PreferJpeg;
    config.keep_unicode_in_filenames = true;
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::parse_from_rfc3339("2020-06-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    ));
    config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    ));
    config.recent = Some(500);
    config.force_resolution = true;
    config.media.videos = false;
    config.live_photo_mode = LivePhotoMode::ImageOnly;
    config.filename_exclude = std::sync::Arc::from(vec![
        glob::Pattern::new("*.AAE").unwrap(),
        glob::Pattern::new("*.THM").unwrap(),
    ]);
    let hash = hash_download_config(&config);
    assert_eq!(
        hash, "4d5a9f1a5a0bf903",
        "hash_download_config golden hash changed -- this will trigger full re-syncs"
    );
    assert_eq!(
        hash_legacy_download_config(&config),
        "d327fda31e8bec04",
        "legacy hash changed -- matching stored hashes would no longer migrate safely"
    );
}

#[test]
fn golden_compute_config_hash_defaults() {
    let tmp = TempDir::new().unwrap();
    let config = build_config_with(tmp.path(), "/photos", |_| {});
    let hash = compute_config_hash(&config);
    assert_eq!(
        hash, "9c00642f0507dce7",
        "compute_config_hash golden hash changed -- this will invalidate sync tokens"
    );
}

#[test]
fn golden_compute_config_hash_with_albums() {
    let tmp = TempDir::new().unwrap();
    let config = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.albums = vec![
            "Favorites".to_string(),
            "Travel".to_string(),
            "!Hidden".to_string(),
        ];
    });
    let hash = compute_config_hash(&config);
    assert_eq!(
        hash, "9c00642f0507dce7",
        "album selection should not change the CloudKit zone-token hash"
    );
}

#[test]
fn golden_compute_config_hash_with_smart_folders() {
    // Smart-folder selection is intentionally excluded from the token
    // safety hash. This pins that selection-only changes stay stable.
    let tmp = TempDir::new().unwrap();
    let config = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.smart_folders = vec!["Favorites".to_string(), "Videos".to_string()];
    });
    let hash = compute_config_hash(&config);
    assert_eq!(
        hash, "9c00642f0507dce7",
        "smart-folder selection should not change the CloudKit zone-token hash"
    );
}

#[test]
fn golden_compute_config_hash_with_unfiled_false() {
    // Drift detection for the unfiled branch of the hash. The
    // `unfiled = true` default is implicit in `golden_..._defaults`;
    // this pin covers the explicit-false case so a regression
    // collapsing the two branches is caught.
    let tmp = TempDir::new().unwrap();
    let config = build_config_with(tmp.path(), "/photos", |s| {
        s.config_overrides.unfiled = Some(false);
    });
    let hash = compute_config_hash(&config);
    assert_eq!(
        hash, "c9ea2589956cbb98",
        "compute_config_hash golden hash changed -- this will invalidate sync tokens"
    );
}
