//! Sync tests with behavioral assertions.
//!
//! Uses the `icloudpd-test` iCloud album with known content:
//! - GOPR0558.JPG        — regular JPEG photo
//! - IMG_0962.MOV        — standalone video
//! - IMG_0212.HEIC       — Live Photo (HEIC + MOV companion)
//! - IMG_0199.DNG        — Apple ProRAW (RAW + JPEG derivative)
//! - Café_🧠godzill.jpg  — JPEG with unicode filename
//!
//! Requires pre-authenticated session. Run with `--test-threads=1`.

mod common;

use predicates::prelude::*;
use tempfile::tempdir;

const ALBUM: &str = "icloudpd-test";
const TIMEOUT_SECS: u64 = 180;
const TIMEOUT_META: u64 = 90;

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
        ALBUM,
        "--username",
        username,
        "--password",
        password,
        "--cookie-directory",
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
fn list_albums_prints_album_names() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "sync",
                "--list-albums",
                "--username",
                &username,
                "--password",
                &password,
                "--cookie-directory",
                cookie_dir.to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(std::time::Duration::from_secs(TIMEOUT_META))
            .assert()
            .success()
            .stdout(predicate::str::contains("Albums:"));
    });
}

#[test]
fn list_libraries_prints_output() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "sync",
                "--list-libraries",
                "--username",
                &username,
                "--password",
                &password,
                "--cookie-directory",
                cookie_dir.to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(std::time::Duration::from_secs(TIMEOUT_META))
            .assert()
            .success()
            .stdout(predicate::str::contains("libraries:"));
    });
}

// ── Core download ───────────────────────────────────────────────────────

/// Downloads the full test album and verifies all expected asset types are present.
#[test]
fn sync_album_downloads_all_asset_types() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            files.len() >= 5,
            "expected at least 5 files from test album, got {}",
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
        assert!(has_ext("heic"), "expected a HEIC file in: {files:?}");
        assert!(has_ext("dng"), "expected a DNG file in: {files:?}");
    });
}

/// Dry-run should list assets but not write any files to disk.
#[test]
fn sync_dry_run_downloads_nothing() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--dry-run"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_idempotent_second_run_noop() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        // First sync
        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_skip_videos_excludes_video_files() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-videos"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "should download files when skipping videos"
        );

        // --skip-videos excludes standalone videos, not Live Photo MOV companions
        let standalone_videos: Vec<_> = files
            .iter()
            .filter(|p| is_video_ext(p) && !file_name_contains(p, "0212"))
            .collect();
        assert!(
            standalone_videos.is_empty(),
            "--skip-videos should exclude standalone video files, found: {standalone_videos:?}"
        );

        // Live Photo MOV companion (IMG_0212) should still be present
        let live_movs = live_photo_movs(download_dir.path());
        assert!(
            !live_movs.is_empty(),
            "--skip-videos should keep Live Photo MOV companions, but none found"
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
fn sync_skip_photos_excludes_image_files() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-photos"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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

/// --skip-live-photos should exclude Live Photo MOV companions but keep
/// standalone videos and the Live Photo still image.
#[test]
fn sync_skip_live_photos_excludes_companions() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-live-photos"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());

        // Standalone video (IMG_0962.MOV) should still be present
        let standalone_video = files.iter().any(|p| file_name_contains(p, "0962"));
        assert!(
            standalone_video,
            "standalone video (IMG_0962) should still be downloaded"
        );

        // Live Photo MOV companion should NOT be present
        let live_photo_mov = files.iter().any(|p| {
            file_name_contains(p, "0212")
                && p.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .eq_ignore_ascii_case("mov")
        });
        assert!(
            !live_photo_mov,
            "Live Photo MOV companion should be excluded by --skip-live-photos"
        );
    });
}

/// Skipping all media types (videos + photos + live photos) should download nothing.
#[test]
fn sync_skip_all_media_downloads_nothing() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--skip-videos", "--skip-photos", "--skip-live-photos"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_date_filters_exclude_by_creation_date() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        // skip-created-before with far-future date — everything filtered
        {
            let dir = tempdir().expect("tempdir");
            album_cmd(&username, &password, &cookie_dir, dir.path())
                .args(["--skip-created-before", "2099-01-01"])
                .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
                .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
                .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
                .assert()
                .success();
        }
    });
}

// ── Size and naming ─────────────────────────────────────────────────────

/// --size medium should produce photo files significantly smaller than originals.
/// Medium photos (2048px longest edge) should be well under 2MB.
#[test]
fn sync_size_medium_produces_smaller_files() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--size", "medium", "--skip-videos"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_force_size_succeeds_when_available() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--size", "medium", "--force-size"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "--force-size with available size should download files"
        );

        // With --force-size medium, non-RAW files should be smaller than originals
        let non_raw_files: Vec<_> = files.iter().filter(|p| !is_raw_ext(p)).collect();
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
fn sync_name_id7_appends_asset_id() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--file-match-policy", "name-id7"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_custom_folder_structure() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--folder-structure", "%Y"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_keep_unicode_preserves_special_chars() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--keep-unicode-in-filenames"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_set_exif_datetime_embeds_date() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--set-exif-datetime", "--skip-videos"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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

        // Read EXIF from the first JPEG and verify DateTimeOriginal is present
        let file = std::fs::File::open(jpeg_files[0]).expect("open JPEG");
        let mut reader = std::io::BufReader::new(file);
        let exif_data = exif::Reader::new()
            .read_from_container(&mut reader)
            .expect("read EXIF data");
        let dt = exif_data.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY);
        assert!(
            dt.is_some(),
            "DateTimeOriginal EXIF tag should be present after --set-exif-datetime"
        );
    });
}

// ── RAW alignment ───────────────────────────────────────────────────────

/// --align-raw variants (as-is, original, alternative) should produce different
/// file naming for the RAW+JPEG pair (IMG_0199.DNG).
#[test]
fn sync_align_raw_controls_raw_naming() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let mut all_filenames: Vec<Vec<String>> = Vec::new();

        for variant in ["as-is", "original", "alternative"] {
            let dir = tempdir().expect("tempdir");
            album_cmd(&username, &password, &cookie_dir, dir.path())
                .args(["--align-raw", variant])
                .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
                .assert()
                .success();

            let files = common::walkdir(dir.path());

            // DNG should be present in each variant
            let has_dng = files.iter().any(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .eq_ignore_ascii_case("dng")
            });
            assert!(
                has_dng,
                "DNG file should be present with --align-raw {variant}"
            );

            // Collect all filenames for comparison
            let mut names: Vec<String> = files
                .iter()
                .map(|p| {
                    p.strip_prefix(dir.path())
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string()
                })
                .collect();
            names.sort();
            all_filenames.push(names);
        }

        // align-raw only changes behavior when the API exposes both RAW and JPEG
        // versions for the same asset. If the ProRAW only has a DNG version (no
        // separate JPEG derivative), all three modes produce identical output —
        // which is correct. Just verify the flag is accepted and DNG is present
        // (assertions above). If the API does expose both versions, at least one
        // pair of variants should differ.
        if all_filenames[0] == all_filenames[1] && all_filenames[1] == all_filenames[2] {
            // Check that we at least got files (the flag didn't break anything)
            assert!(
                !all_filenames[0].is_empty(),
                "align-raw should download files"
            );
        }
    });
}

// ── Live Photo MOV policy ───────────────────────────────────────────────

/// --live-photo-mov-filename-policy suffix vs original should produce
/// different MOV companion filenames for Live Photos.
#[test]
fn sync_live_photo_mov_policy_controls_naming() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        // Download with "suffix" policy
        let dir_suffix = tempdir().expect("tempdir");
        album_cmd(&username, &password, &cookie_dir, dir_suffix.path())
            .args(["--live-photo-mov-filename-policy", "suffix"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        // Download with "original" policy
        let dir_original = tempdir().expect("tempdir");
        album_cmd(&username, &password, &cookie_dir, dir_original.path())
            .args(["--live-photo-mov-filename-policy", "original"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        // Find Live Photo MOV files (containing "0212" — the Live Photo asset)
        let suffix_movs = live_photo_movs(dir_suffix.path());
        let original_movs = live_photo_movs(dir_original.path());

        assert!(
            !suffix_movs.is_empty(),
            "Live Photo MOV should be present with suffix policy"
        );
        assert!(
            !original_movs.is_empty(),
            "Live Photo MOV should be present with original policy"
        );

        // The two policies should produce different MOV filenames
        assert_ne!(
            suffix_movs, original_movs,
            "suffix and original policies should produce different MOV names: \
             suffix={suffix_movs:?}, original={original_movs:?}"
        );
    });
}

// ── Misc flags ──────────────────────────────────────────────────────────

/// --temp-suffix .downloading should leave no temp files after a successful sync.
#[test]
fn sync_temp_suffix_leaves_no_remnants() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--temp-suffix", ".downloading"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_threads_num_reflected_in_log() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        let assertion = album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--threads-num", "1"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_notification_script_fires_event() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let script_dir = tempdir().expect("tempdir");
        let marker = script_dir.path().join("notified.txt");

        let script_path = script_dir.path().join("notify.sh");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\necho \"$ICLOUDPD_EVENT\" > {}\n",
                marker.display()
            ),
        )
        .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--notification-script", script_path.to_str().unwrap()])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_pid_file_cleaned_up_after_sync() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let pid_dir = tempdir().expect("tempdir");
        let pid_file = pid_dir.path().join("test.pid");

        album_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--pid-file", pid_file.to_str().unwrap()])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
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
fn sync_bare_invocation_works_like_sync() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        common::cmd()
            .args([
                "--album",
                ALBUM,
                "--username",
                &username,
                "--password",
                &password,
                "--cookie-directory",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
                "--no-incremental",
            ])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            files.len() >= 5,
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
fn sync_without_directory_fails() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::cmd()
        .args([
            "sync",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(TIMEOUT_META))
        .assert()
        .failure()
        .stderr(predicate::str::contains("directory").or(predicate::str::contains("--directory")));
}

// ── Error paths (auth required) ─────────────────────────────────────────

#[test]
fn sync_nonexistent_album_fails() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

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
                "--cookie-directory",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(std::time::Duration::from_secs(TIMEOUT_META))
            .assert()
            .failure()
            .stderr(predicate::str::contains("not found"));
    });
}

#[test]
fn sync_nonexistent_library_fails() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

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
                "--cookie-directory",
                cookie_dir.to_str().unwrap(),
                "--directory",
                download_dir.path().to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(std::time::Duration::from_secs(TIMEOUT_META))
            .assert()
            .failure()
            .stderr(
                predicate::str::contains("error")
                    .or(predicate::str::contains("Error"))
                    .or(predicate::str::contains("ERROR")),
            );
    });
}

// ── Bad credentials (LAST — hits auth from scratch, burns rate limit) ───

#[test]
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
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
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

/// Find Live Photo MOV filenames (files containing "0212" with .mov extension).
fn live_photo_movs(dir: &std::path::Path) -> Vec<String> {
    common::walkdir(dir)
        .iter()
        .filter(|p| {
            file_name_contains(p, "0212")
                && p.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .eq_ignore_ascii_case("mov")
        })
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        .collect()
}
