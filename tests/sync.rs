//! Sync command tests — downloading, listing, filtering.
//!
//! Network-dependent tests require `ICLOUD_USERNAME` and `ICLOUD_PASSWORD`.
//! They are skipped when credentials are not available.

mod common;

use predicates::prelude::*;
use tempfile::tempdir;

// ── list-albums ─────────────────────────────────────────────────────────

#[test]
fn list_albums_prints_album_names() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .args([
            "sync",
            "--list-albums",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(90))
        .assert()
        .success()
        .stdout(predicate::str::contains("Albums:"));
}

// ── list-libraries ──────────────────────────────────────────────────────

#[test]
fn list_libraries_prints_output() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .args([
            "sync",
            "--list-libraries",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(90))
        .assert()
        .success()
        .stdout(predicate::str::contains("libraries:"));
}

// ── dry-run ─────────────────────────────────────────────────────────────

#[test]
fn sync_dry_run_downloads_nothing() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--dry-run",
            "--recent",
            "5",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Dry run should not write any photo files (the directory may contain
    // the state .db file via cookie_dir, but download_dir should be empty).
    let files: Vec<_> = walkdir(download_dir.path());
    assert!(
        files.is_empty(),
        "dry-run should not download files, found: {files:?}"
    );
}

// ── small recent download ───────────────────────────────────────────────

#[test]
fn sync_recent_downloads_files() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "2",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success();

    // Should have downloaded at least one file
    let files: Vec<_> = walkdir(download_dir.path());
    assert!(!files.is_empty(), "expected at least one downloaded file");

    // Every downloaded file should be non-empty
    for path in &files {
        let meta = std::fs::metadata(path).expect("file metadata");
        assert!(
            meta.len() > 0,
            "downloaded file should be non-empty: {}",
            path.display()
        );
    }
}

// ── skip-videos / skip-photos ───────────────────────────────────────────

#[test]
fn sync_skip_videos_only_downloads_photos() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "5",
            "--skip-videos",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success();

    // No .mp4 or .mov files should be present
    let video_files: Vec<_> = walkdir(download_dir.path())
        .into_iter()
        .filter(|p| {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            ext == "mp4" || ext == "mov"
        })
        .collect();
    assert!(
        video_files.is_empty(),
        "skip-videos should not download video files, found: {video_files:?}"
    );
}

// ── sync without --directory fails ──────────────────────────────────────

#[test]
fn sync_without_directory_fails() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .args([
            "sync",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(90))
        .assert()
        .failure()
        .stderr(predicate::str::contains("--directory"));
}

// ── sync with invalid album ─────────────────────────────────────────────

#[test]
fn sync_with_nonexistent_album_fails() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

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
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(90))
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

// ── sync with --size medium ─────────────────────────────────────────────

#[test]
fn sync_size_medium_downloads_files() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--size",
            "medium",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files: Vec<_> = walkdir(download_dir.path());
    assert!(
        !files.is_empty(),
        "expected at least one downloaded file with --size medium"
    );
}

// ── sync with --file-match-policy name-id7 ──────────────────────────────

#[test]
fn sync_name_id7_downloads_files() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--file-match-policy",
            "name-id7",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files: Vec<_> = walkdir(download_dir.path());
    assert!(
        !files.is_empty(),
        "expected at least one file with name-id7 policy"
    );
}

// ── sync --skip-photos ──────────────────────────────────────────────────

#[test]
fn sync_skip_photos_downloads_no_images() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "5",
            "--skip-photos",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success();

    // No image files should be present
    let image_files: Vec<_> = walkdir(download_dir.path())
        .into_iter()
        .filter(|p| {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            matches!(
                ext.as_str(),
                "jpg" | "jpeg" | "heic" | "png" | "tiff" | "cr2" | "nef" | "dng"
            )
        })
        .collect();
    assert!(
        image_files.is_empty(),
        "skip-photos should not download image files, found: {image_files:?}"
    );
}

// ── sync --folder-structure custom ──────────────────────────────────────

#[test]
fn sync_custom_folder_structure() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--folder-structure",
            "%Y",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // With "%Y" structure, files should be in year-only directories (e.g., 2025/)
    let files = walkdir(download_dir.path());
    for path in &files {
        let relative = path.strip_prefix(download_dir.path()).unwrap();
        let components: Vec<_> = relative.components().collect();
        // Should be exactly 2 components: year dir + filename
        assert_eq!(
            components.len(),
            2,
            "expected year/filename structure, got: {}",
            relative.display()
        );
    }
}

// ── sync --set-exif-datetime ────────────────────────────────────────────

#[test]
fn sync_set_exif_datetime_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    // Just verify the flag doesn't cause a crash
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--set-exif-datetime",
            "--skip-videos",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files = walkdir(download_dir.path());
    assert!(!files.is_empty(), "expected at least one downloaded file");
}

// ── sync --force-size ───────────────────────────────────────────────────

#[test]
fn sync_force_size_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--size",
            "medium",
            "--force-size",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --align-raw ────────────────────────────────────────────────────

#[test]
fn sync_align_raw_as_is_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--align-raw",
            "as-is",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --skip-created-before ──────────────────────────────────────────

#[test]
fn sync_skip_created_before_filters_old_assets() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    // Use a far-future date so everything is filtered out
    common::cmd()
        .args([
            "sync",
            "--recent",
            "5",
            "--skip-created-before",
            "2099-01-01",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Everything should be filtered out
    let files = walkdir(download_dir.path());
    assert!(
        files.is_empty(),
        "skip-created-before 2099 should filter all assets, found: {files:?}"
    );
}

// ── sync --skip-created-after ───────────────────────────────────────────

#[test]
fn sync_skip_created_after_filters_recent_assets() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    // Use a far-past date so everything is filtered out
    common::cmd()
        .args([
            "sync",
            "--recent",
            "5",
            "--skip-created-after",
            "2000-01-01",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Everything should be filtered out
    let files = walkdir(download_dir.path());
    assert!(
        files.is_empty(),
        "skip-created-after 2000 should filter all assets, found: {files:?}"
    );
}

// ── sync --skip-created-before with interval syntax ─────────────────────

#[test]
fn sync_skip_created_before_interval_syntax() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    // "1d" = skip assets older than 1 day
    common::cmd()
        .args([
            "sync",
            "--recent",
            "2",
            "--skip-created-before",
            "1d",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --keep-unicode-in-filenames ────────────────────────────────────

#[test]
fn sync_keep_unicode_in_filenames_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--keep-unicode-in-filenames",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --skip-live-photos ──────────────────────────────────────────────

#[test]
fn sync_skip_live_photos_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "3",
            "--skip-live-photos",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --temp-suffix ──────────────────────────────────────────────────

#[test]
fn sync_custom_temp_suffix_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--temp-suffix",
            ".downloading",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // No .downloading temp files should remain after a successful sync
    let temp_files: Vec<_> = walkdir(download_dir.path())
        .into_iter()
        .filter(|p| p.to_str().unwrap_or("").ends_with(".downloading"))
        .collect();
    assert!(
        temp_files.is_empty(),
        "no temp files should remain: {temp_files:?}"
    );
}

// ── sync --threads-num 1 ────────────────────────────────────────────────

#[test]
fn sync_single_thread_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--threads-num",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files = walkdir(download_dir.path());
    assert!(
        !files.is_empty(),
        "expected at least one file with --threads-num 1"
    );
}

// ── sync --max-retries 0 ────────────────────────────────────────────────

#[test]
fn sync_max_retries_zero_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--max-retries",
            "0",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --library nonexistent ──────────────────────────────────────────

#[test]
fn sync_nonexistent_library_fails() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--library",
            "NonExistentLibrary-ZZZZZ",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(90))
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

// ── sync --notification-script ──────────────────────────────────────────

#[test]
fn sync_notification_script_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");
    let script_output = tempdir().expect("failed to create script output dir");
    let marker = script_output.path().join("notified.txt");

    // Create a simple notification script that writes a marker file
    let script_path = script_output.path().join("notify.sh");
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
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--notification-script",
            script_path.to_str().unwrap(),
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // The notification script should have been called
    assert!(
        marker.exists(),
        "notification script should have created marker file"
    );
    let content = std::fs::read_to_string(&marker).expect("read marker");
    assert!(
        !content.trim().is_empty(),
        "marker file should contain event name"
    );
}

// ── idempotent re-sync ──────────────────────────────────────────────────

#[test]
fn sync_twice_second_run_is_noop() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    let base_args = [
        "sync",
        "--recent",
        "2",
        "--username",
        &username,
        "--password",
        &password,
        "--cookie-directory",
        cookie_dir.path().to_str().unwrap(),
        "--directory",
        download_dir.path().to_str().unwrap(),
        "--no-progress-bar",
    ];

    // First sync
    common::cmd()
        .args(base_args)
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success();

    let files_after_first = walkdir(download_dir.path());
    assert!(
        !files_after_first.is_empty(),
        "first sync should download files"
    );

    // Capture modification times
    let mtimes_before: Vec<_> = files_after_first
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
        .collect();

    // Second sync — should be a no-op since files are already downloaded
    common::cmd()
        .args(base_args)
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files_after_second = walkdir(download_dir.path());
    assert_eq!(
        files_after_first.len(),
        files_after_second.len(),
        "second sync should not create additional files"
    );

    // Files should not have been re-written
    let mtimes_after: Vec<_> = files_after_second
        .iter()
        .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
        .collect();
    assert_eq!(
        mtimes_before, mtimes_after,
        "files should not be modified on second sync"
    );
}

// ── bare invocation (no subcommand) runtime ─────────────────────────────

#[test]
fn bare_invocation_runtime_works() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    // No "sync" subcommand — bare invocation should work identically
    common::cmd()
        .args([
            "--recent",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files = walkdir(download_dir.path());
    assert!(!files.is_empty(), "bare invocation should download files");
}

// ── sync --skip-videos --skip-photos combined ───────────────────────────

#[test]
fn sync_skip_videos_and_skip_photos_downloads_nothing() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "5",
            "--skip-videos",
            "--skip-photos",
            "--skip-live-photos",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files = walkdir(download_dir.path());
    assert!(
        files.is_empty(),
        "skipping all media types should download nothing, found: {files:?}"
    );
}

// ── sync with bad credentials ───────────────────────────────────────────

#[test]
fn sync_with_bad_credentials_fails() {
    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "sync",
            "--recent",
            "1",
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
        .stderr(predicate::str::is_empty().not());
}

// ── sync --live-photo-mov-filename-policy suffix ────────────────────────

#[test]
fn sync_live_photo_mov_filename_policy_suffix_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--live-photo-mov-filename-policy",
            "suffix",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --live-photo-mov-filename-policy original ──────────────────────

#[test]
fn sync_live_photo_mov_filename_policy_original_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--live-photo-mov-filename-policy",
            "original",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --align-raw original ───────────────────────────────────────────

#[test]
fn sync_align_raw_original_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--align-raw",
            "original",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --align-raw alternative ────────────────────────────────────────

#[test]
fn sync_align_raw_alternative_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--align-raw",
            "alternative",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --retry-delay ──────────────────────────────────────────────────

#[test]
fn sync_retry_delay_succeeds() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--retry-delay",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── sync --pid-file ─────────────────────────────────────────────────────

#[test]
fn sync_pid_file_created_and_removed() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");
    let download_dir = tempdir().expect("failed to create download dir");
    let pid_dir = tempdir().expect("failed to create pid dir");
    let pid_file = pid_dir.path().join("test.pid");

    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--pid-file",
            pid_file.to_str().unwrap(),
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // PID file should be cleaned up after sync completes
    assert!(!pid_file.exists(), "PID file should be removed after sync");
}

// ── Helper ──────────────────────────────────────────────────────────────

/// Recursively collect all regular files under `dir`.
fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}
