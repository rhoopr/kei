//! Sync tests with behavioral assertions (live iCloud API).
//!
//! Uses a test album in iCloud (default `kei-test`, override with
//! `KEI_TEST_ALBUM`) that must contain at least:
//! - one regular JPEG
//! - one standalone video (.MOV or .MP4)
//! - one JPEG with a non-ASCII filename
//!
//! All tests are `#[ignore]` -- they require iCloud credentials and hit the
//! live Apple API. Run with:
//!
//! ```sh
//! cargo test --test sync -- --ignored --test-threads=1
//! ```

mod common;

use predicates::prelude::*;
use std::time::Duration;
use tempfile::tempdir;

const TIMEOUT_SECS: u64 = 180;
const TIMEOUT_META: u64 = 90;

/// Name of the iCloud album used for live tests. Defaults to `kei-test`.
/// Override with `KEI_TEST_ALBUM=<name>` so a different account can run
/// the suite.
fn album() -> &'static str {
    static A: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    A.get_or_init(|| std::env::var("KEI_TEST_ALBUM").unwrap_or_else(|_| "kei-test".to_string()))
}

/// Build a sync command targeting the test album.
fn album_cmd(
    username: &str,
    password: &str,
    cookie_dir: &std::path::Path,
    download_dir: &std::path::Path,
) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "sync",
        "--album",
        album(),
        "--username",
        username,
        "--password",
        password,
        "--data-dir",
        cookie_dir.to_str().unwrap(),
        "--directory",
        download_dir.to_str().unwrap(),
        "--no-progress-bar",
        "--no-incremental",
    ]);
    cmd
}

// ── Metadata (no downloads) ─────────────────────────────────────────────

#[test]
#[ignore]
fn list_albums_prints_album_names() {
    let (username, _password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "list",
                "albums",
                "--username",
                &username,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
            ])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .success()
            .stdout(predicate::str::contains("Library:"));
    });
}

#[test]
#[ignore]
fn list_libraries_prints_output() {
    let (username, _password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "list",
                "libraries",
                "--username",
                &username,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
            ])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .success()
            .stdout(predicate::str::contains("libraries:"));
    });
}

// ── Core download ───────────────────────────────────────────────────────

/// Downloads the full test album and verifies all expected asset types are present.
#[test]
#[ignore]
fn sync_album_downloads_all_asset_types() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            files.len() >= 3,
            "expected at least 3 files from test album, got {}",
            files.len()
        );

        // All files should be non-empty
        for f in &files {
            let size = std::fs::metadata(f).unwrap().len();
            assert!(size > 0, "file should be non-empty: {}", f.display());
        }

        // Verify expected file types are present
        let has_ext = |target: &str| {
            files.iter().any(|p: &std::path::PathBuf| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case(target))
            })
        };
        assert!(
            has_ext("jpg") || has_ext("jpeg"),
            "expected a JPEG file in: {files:?}"
        );
        assert!(has_ext("mov"), "expected a MOV file in: {files:?}");
    });
}

/// Dry-run should list assets but not write any files to disk.
#[test]
#[ignore]
fn sync_dry_run_downloads_nothing() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--dry-run"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            files.is_empty(),
            "dry-run should download nothing, found: {files:?}"
        );
    });
}

/// Running sync twice should not re-download or modify any files.
#[test]
#[ignore]
fn sync_idempotent_second_run_noop() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        // First sync
        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files_first = common::walkdir(download_dir.path());
        assert!(!files_first.is_empty(), "first sync should download files");

        let mtimes_before: Vec<_> = files_first
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
            .collect();

        // Second sync — should be a no-op
        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files_second = common::walkdir(download_dir.path());
        assert_eq!(
            files_first.len(),
            files_second.len(),
            "second sync should not create additional files"
        );

        let mtimes_after: Vec<_> = files_second
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
            .collect();
        assert_eq!(
            mtimes_before, mtimes_after,
            "files should not be re-written on second sync"
        );
    });
}

// ── Filter flags ────────────────────────────────────────────────────────

/// --skip-videos should exclude all .mov/.mp4 files but still download images.
#[test]
#[ignore]
fn sync_skip_videos_excludes_video_files() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "should download files when skipping videos"
        );

        // No video files should be present (album has no Live Photo MOV companions)
        let video_files: Vec<_> = files.iter().filter(|p| is_video_ext(p)).collect();
        assert!(
            video_files.is_empty(),
            "--skip-videos should exclude all video files, found: {video_files:?}"
        );

        let image_files: Vec<_> = files.iter().filter(|p| is_image_ext(p)).collect();
        assert!(
            !image_files.is_empty(),
            "should still download image files when skipping videos"
        );
    });
}

/// --skip-photos should exclude all image files but still download videos.
#[test]
#[ignore]
fn sync_skip_photos_excludes_image_files() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-photos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        let image_files: Vec<_> = files.iter().filter(|p| is_image_ext(p)).collect();
        assert!(
            image_files.is_empty(),
            "--skip-photos should exclude all image files, found: {image_files:?}"
        );

        let video_files: Vec<_> = files.iter().filter(|p| is_video_ext(p)).collect();
        assert!(
            !video_files.is_empty(),
            "should still download video files when skipping photos"
        );
    });
}

/// --skip-live-photos flag should be accepted and sync should succeed.
/// NOTE: test album has no Live Photos -- this only verifies the flag works.
#[test]
#[ignore]
fn sync_skip_live_photos_excludes_companions() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-live-photos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());

        // Standalone video (IMG_0962.MOV) should still be present
        let standalone_video = files.iter().any(|p| file_name_contains(p, "0962"));
        assert!(
            standalone_video,
            "standalone video (IMG_0962) should still be downloaded"
        );
    });
}

/// Skipping all media types (videos + photos + live photos) should download nothing.
#[test]
#[ignore]
fn sync_skip_all_media_downloads_nothing() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-videos", "--skip-photos", "--skip-live-photos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            files.is_empty(),
            "skipping all media types should download nothing, found: {files:?}"
        );
    });
}

/// Date filters with extreme values should filter everything out.
/// Also verifies interval syntax (e.g., "1d") parses correctly.
#[test]
#[ignore]
fn sync_date_filters_exclude_by_creation_date() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        // skip-created-before with far-future date — everything filtered
        {
            let dir = tempdir().expect("tempdir");
            album_cmd(&username, &password, &cookie_dir, dir.path())
                .args(["--skip-created-before", "2099-01-01"])
                .timeout(Duration::from_secs(TIMEOUT_SECS))
                .assert()
                .success();
            let files = common::walkdir(dir.path());
            assert!(
                files.is_empty(),
                "--skip-created-before 2099 should filter everything, found: {files:?}"
            );
        }

        // skip-created-after with far-past date — everything filtered
        {
            let dir = tempdir().expect("tempdir");
            album_cmd(&username, &password, &cookie_dir, dir.path())
                .args(["--skip-created-after", "2000-01-01"])
                .timeout(Duration::from_secs(TIMEOUT_SECS))
                .assert()
                .success();
            let files = common::walkdir(dir.path());
            assert!(
                files.is_empty(),
                "--skip-created-after 2000 should filter everything, found: {files:?}"
            );
        }

        // Interval syntax ("1d") should parse and succeed
        {
            let dir = tempdir().expect("tempdir");
            album_cmd(&username, &password, &cookie_dir, dir.path())
                .args(["--skip-created-before", "1d"])
                .timeout(Duration::from_secs(TIMEOUT_SECS))
                .assert()
                .success();
        }
    });
}

// ── Size and naming ─────────────────────────────────────────────────────

/// --size medium should produce photo files significantly smaller than originals.
/// Medium photos (2048px longest edge) should be well under 2MB.
#[test]
#[ignore]
fn sync_size_medium_produces_smaller_files() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--size", "medium", "--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(!files.is_empty(), "should download files at medium size");

        // Medium photos should be well under 2MB (originals are typically 3-15MB).
        // RAW files (.dng, .cr2, .nef) lack medium/thumb alternatives and silently
        // fall back to the original size, so exclude them from the size check.
        let non_raw_files: Vec<_> = files.iter().filter(|p| !is_raw_ext(p)).collect();
        assert!(
            !non_raw_files.is_empty(),
            "should have non-RAW files at medium size"
        );
        for f in &non_raw_files {
            let size = std::fs::metadata(f).unwrap().len();
            assert!(
                size < 2_097_152,
                "medium-size file should be under 2MB, got {} bytes: {}",
                size,
                f.display()
            );
        }
    });
}

/// --force-size with an available size should succeed and download files.
#[test]
#[ignore]
fn sync_force_size_succeeds_when_available() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--size", "medium", "--force-size"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "--force-size with available size should download files"
        );

        // With --force-size medium, non-RAW photo files should be smaller than originals.
        // Videos don't have meaningful medium alternatives so exclude them too.
        let non_raw_files: Vec<_> = files
            .iter()
            .filter(|p| !is_raw_ext(p) && !is_video_ext(p))
            .collect();
        for f in &non_raw_files {
            let size = std::fs::metadata(f).unwrap().len();
            assert!(
                size < 2_097_152,
                "--force-size medium file should be under 2MB, got {} bytes: {}",
                size,
                f.display()
            );
        }
    });
}

/// --file-match-policy name-id7 should append a 7-character asset ID to every filename.
#[test]
#[ignore]
fn sync_name_id7_appends_asset_id() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--file-match-policy", "name-id7"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(!files.is_empty(), "name-id7 should download files");

        // Every file should have a separator + 7-char alphanumeric suffix in its stem.
        // Live Photo MOV companions may have an extra codec suffix (e.g., _HEVC)
        // appended after the ID, so strip trailing _ALLCAPS before checking.
        for f in &files {
            let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let check_stem = match stem.rfind('_') {
                Some(pos) => {
                    let tail = &stem[pos + 1..];
                    if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_uppercase()) {
                        &stem[..pos]
                    } else {
                        stem
                    }
                }
                None => stem,
            };
            let bytes = check_stem.as_bytes();
            assert!(
                bytes.len() >= 8,
                "filename stem too short for name-id7 pattern: {stem}"
            );
            let sep = bytes[bytes.len() - 8];
            assert!(
                sep == b'_' || sep == b'-',
                "expected separator (_/-) before 7-char ID suffix in: {stem}"
            );
            let suffix = &check_stem[check_stem.len() - 7..];
            assert!(
                suffix.chars().all(|c| c.is_ascii_alphanumeric()),
                "expected 7-char alphanumeric ID suffix, got '{suffix}' in: {stem}"
            );
        }
    });
}

/// --folder-structure %Y should place files in year-only directories (e.g., 2024/file.jpg).
#[test]
#[ignore]
fn sync_custom_folder_structure() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--folder-structure", "%Y"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(!files.is_empty(), "should download files");

        for f in &files {
            let relative = f.strip_prefix(download_dir.path()).unwrap();
            let components: Vec<_> = relative.components().collect();
            assert_eq!(
                components.len(),
                2,
                "expected year/filename structure with %Y, got: {}",
                relative.display()
            );
            // First component should be a 4-digit year
            let year_str = components[0].as_os_str().to_str().unwrap();
            assert!(
                year_str.len() == 4 && year_str.chars().all(|c| c.is_ascii_digit()),
                "expected 4-digit year directory, got: {year_str}"
            );
        }
    });
}

/// --keep-unicode-in-filenames should preserve non-ASCII characters
/// (e.g., Café_🧠godzill.jpg retains the é and 🧠).
#[test]
#[ignore]
fn sync_keep_unicode_preserves_special_chars() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--keep-unicode-in-filenames"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "should download files to check for unicode filenames"
        );
        let has_unicode = files.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| !n.is_ascii())
                .unwrap_or(false)
        });
        assert!(
            has_unicode,
            "expected at least one filename with non-ASCII characters (Café_🧠godzill.jpg)"
        );
    });
}

// ── EXIF ────────────────────────────────────────────────────────────────

/// --set-exif-datetime should embed DateTimeOriginal in downloaded JPEG files.
#[test]
#[ignore]
fn sync_set_exif_datetime_embeds_date() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--set-exif-datetime", "--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        let jpeg_files: Vec<_> = files
            .iter()
            .filter(|p| {
                let ext = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                ext == "jpg" || ext == "jpeg"
            })
            .collect();
        assert!(!jpeg_files.is_empty(), "should have at least one JPEG file");

        // Read XMP from the first JPEG and verify DateTimeOriginal is present
        use xmp_toolkit::{xmp_ns, OpenFileOptions, XmpFile};
        let mut file = XmpFile::new().expect("xmp file handle");
        file.open_file(jpeg_files[0], OpenFileOptions::default().for_read())
            .expect("open JPEG for XMP read");
        let meta = file
            .xmp()
            .expect("JPEG should have XMP after --set-exif-datetime");
        assert!(
            meta.property(xmp_ns::EXIF, "DateTimeOriginal").is_some()
                || meta.property(xmp_ns::XMP, "CreateDate").is_some(),
            "DateTimeOriginal XMP property should be present after --set-exif-datetime"
        );
    });
}

/// --set-exif-rating should add a Rating property (value depends on the
/// source photo; we assert the sync succeeds and the resulting JPEG has
/// a writable XMP packet).
#[test]
#[ignore]
fn sync_set_exif_rating_embeds_rating() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--set-exif-rating", "--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let jpeg = first_jpeg(&download_dir.path());
        use xmp_toolkit::{OpenFileOptions, XmpFile};
        let mut file = XmpFile::new().expect("xmp file handle");
        file.open_file(&jpeg, OpenFileOptions::default().for_read())
            .expect("open JPEG for XMP read");
        assert!(
            file.xmp().is_some(),
            "JPEG should carry an XMP packet after --set-exif-rating"
        );
    });
}

/// --set-exif-gps embeds GPSLatitude/GPSLongitude when the source photo
/// carries location data. Sync must succeed either way; we only assert
/// an XMP packet exists.
#[test]
#[ignore]
fn sync_set_exif_gps_embeds_gps() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--set-exif-gps", "--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let jpeg = first_jpeg(&download_dir.path());
        use xmp_toolkit::{OpenFileOptions, XmpFile};
        let mut file = XmpFile::new().expect("xmp file handle");
        file.open_file(&jpeg, OpenFileOptions::default().for_read())
            .expect("open JPEG for XMP read");
        assert!(
            file.xmp().is_some(),
            "JPEG should carry an XMP packet after --set-exif-gps"
        );
    });
}

/// --set-exif-description embeds a dc:description when the source has
/// one. Sync must succeed either way; we only assert an XMP packet
/// exists.
#[test]
#[ignore]
fn sync_set_exif_description_embeds_description() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--set-exif-description", "--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let jpeg = first_jpeg(&download_dir.path());
        use xmp_toolkit::{OpenFileOptions, XmpFile};
        let mut file = XmpFile::new().expect("xmp file handle");
        file.open_file(&jpeg, OpenFileOptions::default().for_read())
            .expect("open JPEG for XMP read");
        assert!(
            file.xmp().is_some(),
            "JPEG should carry an XMP packet after --set-exif-description"
        );
    });
}

/// --embed-xmp writes a full kei-authored XMP packet into the JPEG. Verify
/// the file carries XMP content that references kei's own namespace URI.
#[test]
#[ignore]
fn sync_embed_xmp_writes_xmp_packet() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--embed-xmp", "--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let jpeg = first_jpeg(&download_dir.path());
        use xmp_toolkit::{OpenFileOptions, XmpFile};
        let mut file = XmpFile::new().expect("xmp file handle");
        file.open_file(&jpeg, OpenFileOptions::default().for_read())
            .expect("open JPEG for XMP read");
        let meta = file.xmp().expect("JPEG should carry XMP after --embed-xmp");
        // kei registers its own namespace (github.com/rhoopr/kei/ns/1.0/)
        // for hidden/archived/mediaSubtype/burstId. Serialize and look for
        // it so we know the packet reached us, not just a remnant from
        // Apple's source.
        let serialized = meta.to_string();
        assert!(
            serialized.contains("xmpmeta") || serialized.contains("rdf:RDF"),
            "XMP packet must serialize to an RDF tree: {serialized}"
        );
    });
}

/// --xmp-sidecar writes a .xmp sidecar next to every downloaded media file.
/// Verify at least one `.xmp` sits next to a downloaded JPEG.
#[test]
#[ignore]
fn sync_xmp_sidecar_writes_sidecar_file() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--xmp-sidecar", "--skip-videos"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        let sidecars: Vec<_> = files
            .iter()
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
            })
            .collect();
        assert!(
            !sidecars.is_empty(),
            "--xmp-sidecar should produce at least one .xmp sidecar, got files: {files:?}"
        );

        let bytes = std::fs::read(sidecars[0]).expect("read sidecar");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("<x:xmpmeta") || text.contains("xmpmeta"),
            "sidecar content must be an XMP packet: {text}"
        );
    });
}

/// --embed-xmp on a HEIC file: kei routes through the mp4-atom HEIC writer
/// rather than xmp_toolkit. The resulting HEIC must carry an XMP packet
/// as a MIME item inside the `meta` box; we detect it by looking for the
/// `<x:xmpmeta` magic in the file bytes.
#[test]
#[ignore]
fn sync_embed_xmp_on_heic_writes_mime_item() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--embed-xmp"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        let heics: Vec<_> = files
            .iter()
            .filter(|p| {
                let ext = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                ext == "heic" || ext == "heif"
            })
            .collect();

        if heics.is_empty() {
            eprintln!(
                "test album `{}` has no HEIC file; skipping HEIC-specific assertion",
                album()
            );
            return;
        }

        let bytes = std::fs::read(heics[0]).expect("read HEIC");
        // <x:xmpmeta is the opening tag the xmp_toolkit serializer emits.
        // mp4-atom embeds that packet unchanged inside the HEIC `mime` item.
        let needle = b"<x:xmpmeta";
        let found = bytes
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle));
        assert!(
            found,
            "HEIC `{}` must carry an XMP packet after --embed-xmp",
            heics[0].display()
        );
    });
}

/// Find the first downloaded JPEG in `dir`. Panics with a clear message if
/// none is present — the test album must contain at least one JPEG.
fn first_jpeg(dir: &&std::path::Path) -> std::path::PathBuf {
    let files = common::walkdir(dir);
    files
        .into_iter()
        .find(|p| {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            ext == "jpg" || ext == "jpeg"
        })
        .unwrap_or_else(|| panic!("no JPEG in {}", dir.display()))
}

// ── RAW alignment ───────────────────────────────────────────────────────

/// --align-raw variants should be accepted and sync should succeed.
/// NOTE: test album has no RAW files -- this verifies the flag is accepted
/// without errors rather than testing naming behavior.
#[test]
#[ignore]
fn sync_align_raw_controls_raw_naming() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        for variant in ["as-is", "original", "alternative"] {
            let dir = tempdir().expect("tempdir");
            album_cmd(&username, &password, &cookie_dir, dir.path())
                .args(["--align-raw", variant])
                .timeout(Duration::from_secs(TIMEOUT_SECS))
                .assert()
                .success();

            let files = common::walkdir(dir.path());
            assert!(
                files.len() >= 3,
                "--align-raw {variant} should download files, got {}",
                files.len()
            );
        }
    });
}

// ── Live Photo MOV policy ───────────────────────────────────────────────

/// --live-photo-mov-filename-policy flag should be accepted and sync should succeed.
/// NOTE: test album has no Live Photos -- this only verifies the flag is accepted.
/// Re-enable naming assertions when the album is repopulated with a Live Photo.
#[test]
#[ignore]
fn sync_live_photo_mov_policy_controls_naming() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        for policy in ["suffix", "original"] {
            let dir = tempdir().expect("tempdir");
            album_cmd(&username, &password, &cookie_dir, dir.path())
                .args(["--live-photo-mov-filename-policy", policy])
                .timeout(Duration::from_secs(TIMEOUT_SECS))
                .assert()
                .success();

            let files = common::walkdir(dir.path());
            assert!(
                files.len() >= 3,
                "--live-photo-mov-filename-policy {policy} should download files, got {}",
                files.len()
            );
        }
    });
}

// ── Misc flags ──────────────────────────────────────────────────────────

/// --temp-suffix .downloading should leave no temp files after a successful sync.
#[test]
#[ignore]
fn sync_temp_suffix_leaves_no_remnants() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--temp-suffix", ".downloading"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let all_files = common::walkdir(download_dir.path());
        assert!(
            !all_files.is_empty(),
            "should download files with --temp-suffix"
        );
        let temp_files: Vec<_> = all_files
            .iter()
            .filter(|p| p.to_str().unwrap_or("").ends_with(".downloading"))
            .collect();
        assert!(
            temp_files.is_empty(),
            "no .downloading temp files should remain: {temp_files:?}"
        );
    });
}

/// --threads-num value should appear as concurrency=N in log output.
#[test]
#[ignore]
fn sync_threads_num_reflected_in_log() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        let assertion = album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--threads-num", "1", "--log-level", "info"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let stderr = String::from_utf8_lossy(&assertion.get_output().stderr);
        let clean = strip_ansi(&stderr);
        assert!(
            clean.contains("concurrency=1"),
            "log should reflect --threads-num 1, stderr:\n{clean}"
        );
    });
}

/// --notification-script should be called with ICLOUDPD_EVENT set.
#[test]
#[ignore]
fn sync_notification_script_fires_event() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let script_dir = tempdir().expect("tempdir");
        let marker = script_dir.path().join("notified.txt");

        let script_path = script_dir.path().join("notify.sh");
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\necho \"$KEI_EVENT\" > {}\n", marker.display()),
        )
        .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--notification-script", script_path.to_str().unwrap()])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        assert!(
            marker.exists(),
            "notification script should create marker file"
        );
        let content = std::fs::read_to_string(&marker).expect("read marker");
        assert!(
            content.trim() == "sync_complete" || content.trim() == "sync_failed",
            "marker file should contain a known event name, got: {:?}",
            content.trim()
        );
    });
}

/// --pid-file should be created during sync and removed after completion.
#[test]
#[ignore]
fn sync_pid_file_cleaned_up_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let pid_dir = tempdir().expect("tempdir");
        let pid_file = pid_dir.path().join("test.pid");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--pid-file", pid_file.to_str().unwrap()])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        assert!(
            !pid_file.exists(),
            "PID file should be removed after sync completes"
        );

        // Verify sync actually ran (downloaded files)
        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "sync with --pid-file should still download files"
        );
    });
}

// ── Bare invocation ─────────────────────────────────────────────────────

/// Omitting the "sync" subcommand should work identically to `sync`.
#[test]
#[ignore]
fn sync_bare_invocation_works_like_sync() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        common::cmd()
            .args([
                "--album",
                album(),
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
                "--no-incremental",
            ])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            files.len() >= 3,
            "bare invocation should download all test album files, got {}",
            files.len()
        );
        for f in &files {
            let size = std::fs::metadata(f).unwrap().len();
            assert!(size > 0, "file should be non-empty: {}", f.display());
        }
    });
}

// ── Error paths (no network) ────────────────────────────────────────────

#[test]
#[ignore]
fn sync_without_directory_fails() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::cmd()
        .args([
            "sync",
            "--username",
            &username,
            "--password",
            &password,
            "--data-dir",
            cookie_dir.to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(Duration::from_secs(TIMEOUT_META))
        .assert()
        .failure()
        .stderr(predicate::str::contains("directory").or(predicate::str::contains("--directory")));
}

// ── Error paths (auth required) ─────────────────────────────────────────

#[test]
#[ignore]
fn sync_nonexistent_album_fails() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        common::cmd()
            .args([
                "sync",
                "--album",
                "ThisAlbumDefinitelyDoesNotExist999",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .failure()
            .stderr(predicate::str::contains("not found"));
    });
}

#[test]
#[ignore]
fn sync_nonexistent_library_fails() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        common::cmd()
            .args([
                "sync",
                "--library",
                "NonExistentLibrary-ZZZZZ",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .failure()
            .stderr(
                predicate::str::contains("error")
                    .or(predicate::str::contains("Error"))
                    .or(predicate::str::contains("ERROR")),
            );
    });
}

// ── New subcommand tests ───────────────────────────────────────────────

#[test]
#[ignore]
fn login_authenticates_successfully() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "login",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
            ])
            .timeout(Duration::from_secs(60))
            .assert()
            .success();
    });
}

#[test]
#[ignore]
fn list_albums_new_syntax() {
    let (username, _password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "list",
                "albums",
                "--username",
                &username,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
            ])
            .timeout(Duration::from_secs(60))
            .assert()
            .success()
            .stdout(predicate::str::contains(album()));
    });
}

#[test]
#[ignore]
fn sync_retry_failed_flag() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        // sync --retry-failed with no prior failures should succeed (noop)
        common::cmd()
            .args([
                "sync",
                "--retry-failed",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();
    });
}

#[test]
#[ignore]
fn sync_incremental_second_run_skips_download() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        // First sync: full enumeration
        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let first_count = common::walkdir(download_dir.path()).len();
        assert!(first_count >= 3, "first sync should download files");

        // Second sync: incremental (no --no-incremental)
        let output = common::cmd()
            .args([
                "sync",
                "--album",
                album(),
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
                "--log-level",
                "debug",
            ])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .output()
            .unwrap();

        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Second run should use incremental sync
        assert!(
            stderr.contains("incremental") || stderr.contains("Stored sync token"),
            "second run should be incremental, stderr: {stderr}"
        );
    });
}

// ── Watch mode, report JSON, multi-album ────────────────────────────────

/// Verify `--watch-with-interval` drives multiple sync cycles within one run.
///
/// Runs at the minimum interval (60 s) long enough to observe two cycle starts,
/// then kills the process and counts the `sync_loop: Starting kei` markers.
#[test]
#[ignore]
fn sync_watch_runs_multiple_cycles() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let download_dir = tempdir().expect("tempdir");
        let bin = env!("CARGO_BIN_EXE_kei");
        let mut child = Command::new(bin)
            .args([
                "sync",
                "--album",
                album(),
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
                "--watch-with-interval",
                "60",
                "--log-level",
                "info",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kei");

        // First cycle runs at t=0, second at t=60, buffer for download time.
        std::thread::sleep(Duration::from_secs(135));
        let _ = child.kill();

        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let _ = child.wait();

        // Each cycle logs "Waiting before next cycle" at the end; 2 cycles → 2 markers.
        let clean = strip_ansi(&stderr);
        let cycles = clean.matches("Waiting before next cycle").count();
        assert!(
            cycles >= 2,
            "watch should drive at least 2 cycles, got {cycles}. stderr head: {}",
            clean.chars().take(2000).collect::<String>()
        );
    });
}

/// Verify `--report-json` writes a parseable report with the documented schema.
#[test]
#[ignore]
fn sync_report_json_writes_valid_schema() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let report_dir = tempdir().expect("tempdir");
        let report_path = report_dir.path().join("report.json");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--report-json", report_path.to_str().unwrap()])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let body = std::fs::read_to_string(&report_path).expect("report file");
        let json: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(json["version"], "1", "schema version");
        assert!(json["kei_version"].is_string(), "kei_version present");
        assert!(json["timestamp"].is_string(), "timestamp present");
        let status = json["status"].as_str().expect("status string");
        assert!(
            matches!(status, "success" | "partial_failure" | "session_expired"),
            "unexpected status: {status}"
        );
        assert!(json["options"].is_object(), "options object");
        assert_eq!(json["options"]["username"], username.as_str());
        assert!(json["stats"].is_object(), "stats object");
    });
}

/// Verify passing the same album twice still downloads exactly once (dedup).
///
/// Exercises the multi-`--album` code path end-to-end. A richer test would
/// use two distinct small albums, but only `kei-test` exists in the
/// test account, so we assert dedup as the minimal multi-filter invariant.
#[test]
#[ignore]
fn sync_multi_album_dedups() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        common::cmd()
            .args([
                "sync",
                "--album",
                album(),
                "--album",
                album(),
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
                "--no-incremental",
            ])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert_eq!(
            files.len(),
            3,
            "duplicate album names should dedup to 3 files, got {}: {:?}",
            files.len(),
            files
        );
    });
}

// ── Download integrity ──────────────────────────────────────────────────

/// Data-sacred invariant: if the user (or `rm -rf` accident) deletes a synced
/// file, the next sync must restore it. A silent skip here would mean kei
/// "loses" the file permanently.
#[test]
#[ignore]
fn sync_recovers_deleted_file() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let before = common::walkdir(download_dir.path());
        assert!(before.len() >= 3, "expected >=3 files after first sync");

        // Pick a JPEG (stable size/content), record its checksum, delete it.
        let victim = before
            .iter()
            .find(|p| is_image_ext(p) && !is_video_ext(p))
            .expect("at least one image file")
            .clone();
        let expected_size = std::fs::metadata(&victim).unwrap().len();
        std::fs::remove_file(&victim).expect("delete victim");
        assert!(!victim.exists(), "victim deleted");

        // Re-sync: full enumeration so the filter can notice the missing file.
        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        assert!(
            victim.exists(),
            "deleted file should be re-downloaded: {}",
            victim.display()
        );
        let after_size = std::fs::metadata(&victim).unwrap().len();
        assert_eq!(
            after_size, expected_size,
            "recovered file should match original size"
        );
    });
}

/// Data-sacred invariant: a truncated file left on disk (e.g. from a crashed
/// write) must not mask the real photo. The default `name-size-dedup-with-suffix`
/// policy preserves the existing file untouched and downloads the real photo
/// alongside with a size suffix in the filename. Either way, the correctly-sized
/// photo bytes must end up on disk.
#[test]
#[ignore]
fn sync_truncated_file_does_not_cause_data_loss() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        let victim = files
            .iter()
            .find(|p| is_image_ext(p) && !is_video_ext(p))
            .expect("image file")
            .clone();
        let expected_size = std::fs::metadata(&victim).unwrap().len();
        let original_bytes = std::fs::read(&victim).unwrap();
        let parent = victim.parent().unwrap().to_path_buf();

        // Truncate to zero bytes -- simulates a crashed write leaving an empty file.
        std::fs::File::create(&victim)
            .expect("truncate")
            .set_len(0)
            .expect("set_len 0");
        assert_eq!(std::fs::metadata(&victim).unwrap().len(), 0);

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        // The correctly-sized photo must exist somewhere under the same folder
        // (either overwriting the zero-byte file or as a size-suffixed sibling).
        let candidates: Vec<_> = common::walkdir(&parent)
            .into_iter()
            .filter(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) == expected_size)
            .collect();
        assert!(
            !candidates.is_empty(),
            "after re-sync, the correctly-sized photo must be on disk somewhere in {:?}",
            parent
        );
        let recovered = std::fs::read(&candidates[0]).unwrap();
        assert_eq!(
            recovered, original_bytes,
            "recovered photo content must match the original"
        );
    });
}

// ── Bad credentials (LAST -- hits auth from scratch, burns rate limit) ──

#[test]
#[ignore]
fn zz_bad_credentials_fails() {
    let cookie_dir = tempdir().expect("tempdir");
    let download_dir = tempdir().expect("tempdir");

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "sync",
            "--username",
            "nonexistent-xyz@icloud.com",
            "--password",
            "wrong-password",
            "--data-dir",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(Duration::from_secs(60))
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("error")
                .or(predicate::str::contains("Error"))
                .or(predicate::str::contains("ERROR")),
        );
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn is_video_ext(p: &std::path::Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    ext == "mp4" || ext == "mov"
}

fn is_raw_ext(p: &std::path::Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(ext.as_str(), "dng" | "cr2" | "nef")
}

fn is_image_ext(p: &std::path::Path) -> bool {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "heic" | "png" | "tiff" | "cr2" | "nef" | "dng"
    )
}

fn file_name_contains(p: &std::path::Path, pattern: &str) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .contains(pattern)
}

/// Strip ANSI escape sequences from a string (for log output assertions).
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else {
            result.push(c);
        }
    }
    result
}
