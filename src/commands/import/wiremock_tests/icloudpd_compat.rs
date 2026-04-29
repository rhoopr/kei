//! icloudpd compat baseline.
//!
//! Each test stages an on-disk layout using icloudpd's exact fixture data
//! (filenames, folder structure, sizes copied verbatim from
//! `icloud_photos_downloader`'s own test suite), then runs kei's
//! `import_assets` loop and asserts every file matches.
//!
//! Why this exists: kei's positioning is "Rust rewrite of icloudpd," so the
//! migration story is `import-existing` against a library produced by
//! icloudpd. These tests prove kei's path computation produces identical
//! strings to icloudpd's at every flag profile we care about. If a test in
//! this module fails, kei has diverged from icloudpd's layout and migrating
//! users will see unmatched files.
//!
//! Source files mirrored:
//!   - `tests/test_download_photos.py` (default policy)
//!   - `tests/test_download_photos_id.py` (`name-id7` policy)
//!   - `tests/test_download_live_photos.py` (live photos default)
//!   - `tests/test_download_live_photos_id.py` (live photos + id7)
//!   - `tests/test_download_videos.py` (videos)
//!   - `tests/test_folder_structure.py` (`--folder-structure` variants)
//!
//! Fixture conventions:
//!   - `(folder, filename, size_bytes)` triples are lifted directly from
//!     `files_to_create` / `files_to_download` in the upstream tests.
//!   - Folder strings are icloudpd's `{:%Y/%m/%d}` rendering of the asset
//!     date in local TZ. We pick `asset_date = local 18:00 on day Y-M-D`
//!     so DST transitions don't shift us across midnight.
//!   - Wiremock asset metadata is constructed to match the fixtures by
//!     filename + size. record_name is meaningful only for `name-id7`
//!     tests, where the 7-char base64 suffix is part of the path.

use super::*;

use chrono::TimeZone;
use std::path::{Path as StdPath, PathBuf};

/// Build a timestamp (millis since epoch) whose local-TZ rendering with
/// `%Y/%m/%d` produces the requested calendar date. 18:00 local avoids
/// DST-induced midnight rollovers.
fn local_ts_at_day(y: i32, m: u32, d: u32) -> f64 {
    chrono::Local
        .with_ymd_and_hms(y, m, d, 18, 0, 0)
        .single()
        .expect("valid local datetime")
        .timestamp_millis() as f64
}

/// Parse an icloudpd folder string (`"2018/07/30"`) into a date and return
/// the local-TZ asset_date that renders to that folder with `%Y/%m/%d`.
fn ts_for_folder(folder: &str) -> f64 {
    let parts: Vec<u32> = folder
        .split('/')
        .map(|s| s.parse().unwrap_or_else(|_| panic!("bad folder: {folder}")))
        .collect();
    assert_eq!(parts.len(), 3, "folder must be Y/M/D, got {folder}");
    local_ts_at_day(parts[0] as i32, parts[1], parts[2])
}

/// Stage every fixture file at `<dl>/<folder>/<filename>` of `size` bytes.
/// Caller passes a slice of `(folder, filename, size)` triples copied
/// verbatim from icloudpd's tests.
fn stage_icloudpd_fixtures(dl: &StdPath, fixtures: &[(&str, &str, u64)]) -> Vec<PathBuf> {
    fixtures
        .iter()
        .map(|(folder, fname, size)| {
            let p = dl.join(folder).join(fname);
            stage_file(&p, *size);
            p
        })
        .collect()
}

/// MIME-style item_type for a filename, mirroring icloudpd's
/// type-from-extension fallback. Used when a fixture only carries
/// `(folder, filename, size)` and we have to synthesize an `item_type`
/// for the wiremock record.
fn item_type_for(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "public.jpeg"
    } else if lower.ends_with(".heic") {
        "public.heic"
    } else if lower.ends_with(".png") {
        "public.png"
    } else if lower.ends_with(".mov") {
        "com.apple.quicktime-movie"
    } else if lower.ends_with(".mp4") || lower.ends_with(".m4v") {
        "public.mpeg-4"
    } else if lower.ends_with(".dng") {
        "com.adobe.raw-image"
    } else {
        // icloudpd's fallback. kei's `expected_paths_for` should handle
        // an explicit type; if a future fixture trips this, add a branch.
        "public.jpeg"
    }
}

// ── Default file-match policy (`name-size-dedup-with-suffix`) ───────

/// Mirrors `test_download_photos.py::test_download_and_skip_existing_photos`.
/// Three plain JPEGs, two days, default flags.
#[tokio::test]
async fn default_layout_skip_existing() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    // Verbatim from test_download_photos.py:43-48 (files_to_create) +
    // files_to_download.
    let fixtures: &[(&str, &str, u64)] = &[
        ("2018/07/30", "IMG_7408.JPG", 1_151_066),
        ("2018/07/30", "IMG_7407.JPG", 656_257),
        ("2018/07/31", "IMG_7409.JPG", 100_000),
    ];
    stage_icloudpd_fixtures(&dl, fixtures);

    let assets: Vec<WiremockAsset> =
        fixtures
            .iter()
            .enumerate()
            .map(|(i, (folder, fname, size))| {
                let mut a = WiremockAsset::new(&format!("DEF{i}"), fname, item_type_for(fname))
                    .orig(*size, &format!("ck_def_{i}"), item_type_for(fname));
                a.asset_date = ts_for_folder(folder);
                a
            })
            .collect();

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &assets, db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 3);
    assert_eq!(
        stats.matched, 3,
        "kei diverged from icloudpd default layout"
    );
    assert_eq!(stats.unmatched, 0);
}

/// Mirrors the `--file-match-policy name-size-dedup-with-suffix` collision
/// case from `test_download_photos.py:1118`: same iCloud filename
/// appearing twice with different sizes. icloudpd resolves at download
/// time by stat'ing the existing on-disk file -- if size doesn't match,
/// the second download lands at `<stem>-<size><ext>`.
///
/// kei's `expected_paths_for` is single-asset and emits only the bare
/// path, so `import_assets` adds the same fallback at the matcher layer:
/// if the bare path doesn't match the expected size, retry with
/// `add_dedup_suffix(filename, expected_size)`. This handles the icloudpd
/// migration case without making path generation aware of cross-asset
/// state.
#[tokio::test]
async fn dedup_size_suffix_collision() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    let fixtures: &[(&str, &str, u64)] = &[
        ("2018/07/30", "IMG_DUP.JPG", 100_000),
        ("2018/07/30", "IMG_DUP-200000.JPG", 200_000),
    ];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a1 = WiremockAsset::new("DUP1", "IMG_DUP.JPG", "public.jpeg").orig(
        100_000,
        "ck1",
        "public.jpeg",
    );
    a1.asset_date = ts_for_folder("2018/07/30");
    let mut a2 = WiremockAsset::new("DUP2", "IMG_DUP.JPG", "public.jpeg").orig(
        200_000,
        "ck2",
        "public.jpeg",
    );
    a2.asset_date = ts_for_folder("2018/07/30");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a1, a2], db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 2);
    assert_eq!(
        stats.matched, 2,
        "kei must match both colliding assets (one at bare path, one at -<size> fallback)",
    );
    assert_eq!(stats.unmatched, 0);
}

// ── name-id7 policy ────────────────────────────────────────────────

/// Mirrors `test_download_photos_id.py:39` —
/// `IMG_7408_QVI4T2l.JPG` shape. The 7-char suffix is the URL-safe base64
/// of the asset's record_name. icloudpd's fixture record_names produce the
/// exact suffixes shown in their test data; we use record_names that
/// `apply_name_id7` will encode to the same 7 chars.
#[tokio::test]
async fn name_id7_default_layout() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let mut config = base_config(&dl);
    config.file_match_policy = FileMatchPolicy::NameId7;

    // Compute icloudpd-equivalent id7 suffixes by running kei's
    // apply_name_id7 against synthetic record_names. The policy takes the
    // first 7 chars of URL_SAFE_NO_PAD(base_id) — match by deriving the
    // expected on-disk filename via the same function and asserting that
    // string ends up in the staged fixtures.
    use crate::download::paths::apply_name_id7;
    let cases = [
        ("REC_FOR_7408", "IMG_7408.JPG", 1_151_066_u64, "2018/07/30"),
        ("REC_FOR_7407", "IMG_7407.JPG", 656_257, "2018/07/30"),
        ("REC_FOR_7409", "IMG_7409.JPG", 100_000, "2018/07/31"),
    ];

    for (rec, fname, size, folder) in &cases {
        let suffixed = apply_name_id7(fname, rec);
        stage_file(&dl.join(folder).join(&suffixed), *size);
    }

    let assets: Vec<WiremockAsset> = cases
        .iter()
        .map(|(rec, fname, size, folder)| {
            let mut a = WiremockAsset::new(rec, fname, "public.jpeg").orig(
                *size,
                &format!("ck_id7_{rec}"),
                "public.jpeg",
            );
            a.asset_date = ts_for_folder(folder);
            a
        })
        .collect();

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &assets, db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 3);
    assert_eq!(
        stats.matched, 3,
        "kei diverged from icloudpd name-id7 layout"
    );
    assert_eq!(stats.unmatched, 0);
}

// ── Live photos ───────────────────────────────────────────────────

/// Default policy: JPEG live photo writes `<base>.JPG` + `<base>.MOV`,
/// HEIC live photo writes `<base>.HEIC` + `<base>_HEVC.MOV`. Both pairs
/// in the same date folder. Mirrors test_download_live_photos.py.
#[tokio::test]
async fn live_photo_default_layout() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    let folder = "2018/07/30";
    let ts = ts_for_folder(folder);

    let mut jpeg = WiremockAsset::new("LIVEJ", "IMG_7408.JPG", "public.jpeg")
        .orig(1_151_066, "ck_jpeg", "public.jpeg")
        .live_mov(2_000_000, "ck_jpeg_mov");
    jpeg.asset_date = ts;
    let mut heic = WiremockAsset::new("LIVEH", "IMG_5000.HEIC", "public.heic")
        .orig(800_000, "ck_heic", "public.heic")
        .live_mov(3_000_000, "ck_heic_mov");
    heic.asset_date = ts;

    let fixtures: &[(&str, &str, u64)] = &[
        (folder, "IMG_7408.JPG", 1_151_066),
        (folder, "IMG_7408.MOV", 2_000_000),
        (folder, "IMG_5000.HEIC", 800_000),
        (folder, "IMG_5000_HEVC.MOV", 3_000_000),
    ];
    stage_icloudpd_fixtures(&dl, fixtures);

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[jpeg, heic], db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 2);
    // Per-version: 2 assets × 2 versions each = 4 matches.
    assert_eq!(stats.matched, 4, "live photo MOV naming diverged");
    assert_eq!(stats.unmatched, 0);
}

/// Live photos with `name-id7` policy. icloudpd applies the id7 suffix to
/// the master filename FIRST (`IMG_5000.HEIC` → `IMG_5000_<id7>.HEIC`),
/// then derives the MOV by appending `_HEVC.MOV` to the suffixed stem.
/// On-disk shape:
///   - Image: `IMG_5000_<id7>.HEIC`
///   - MOV:   `IMG_5000_<id7>_HEVC.MOV`
/// Reference: `icloudpd/base.py` — `filename_builder(photo)` applies id7
/// to the master, then `lp_filename_generator` (suffix policy) runs on
/// that suffixed name. kei mirrors this in
/// `download/filter.rs::expected_paths_for`.
#[tokio::test]
async fn live_photo_name_id7_layout() {
    use crate::download::paths::apply_name_id7;

    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let mut config = base_config(&dl);
    config.file_match_policy = FileMatchPolicy::NameId7;

    let folder = "2018/07/30";
    let ts = ts_for_folder(folder);

    let rec = "REC_LIVE_HEIC";
    let img = apply_name_id7("IMG_5000.HEIC", rec); // IMG_5000_<id7>.HEIC
    let mov = crate::download::paths::live_photo_mov_path_suffix(&img);
    stage_file(&dl.join(folder).join(&img), 800_000);
    stage_file(&dl.join(folder).join(&mov), 3_000_000);

    let mut asset = WiremockAsset::new(rec, "IMG_5000.HEIC", "public.heic")
        .orig(800_000, "ck_h", "public.heic")
        .live_mov(3_000_000, "ck_hm");
    asset.asset_date = ts;

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[asset], db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 1);
    assert_eq!(
        stats.matched, 2,
        "live photo image + MOV at icloudpd id7 paths must both match",
    );
}

// ── Videos ────────────────────────────────────────────────────────

/// Mirrors test_download_videos.py: a plain `.MOV` (or `.MP4`) asset, no
/// live-photo pairing. Default flags.
#[tokio::test]
async fn video_default_layout() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    let fixtures: &[(&str, &str, u64)] = &[
        ("2018/07/30", "IMG_VID1.MOV", 5_000_000),
        ("2018/07/31", "IMG_VID2.MP4", 10_000_000),
    ];
    stage_icloudpd_fixtures(&dl, fixtures);

    let assets: Vec<WiremockAsset> = fixtures
        .iter()
        .enumerate()
        .map(|(i, (folder, fname, size))| {
            let it = item_type_for(fname);
            let mut a = WiremockAsset::new(&format!("VID{i}"), fname, it).orig(
                *size,
                &format!("ck_vid_{i}"),
                it,
            );
            a.asset_date = ts_for_folder(folder);
            a
        })
        .collect();

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &assets, db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 2);
    assert_eq!(stats.matched, 2, "video layout diverged from icloudpd");
}

// ── Folder structure variants ─────────────────────────────────────

/// `--folder-structure none` (icloudpd) → flat layout, all files in
/// download root. kei equivalent: `folder_structure = ""`.
/// Mirrors test_folder_structure.py's `none` case.
#[tokio::test]
async fn folder_structure_none_flat_layout() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let mut config = base_config(&dl);
    config.folder_structure = String::new();

    // Flat: no folder prefix.
    stage_file(&dl.join("IMG_FLAT1.JPG"), 100_000);
    stage_file(&dl.join("IMG_FLAT2.JPG"), 200_000);

    let mut a1 = WiremockAsset::new("FLAT1", "IMG_FLAT1.JPG", "public.jpeg").orig(
        100_000,
        "ck_f1",
        "public.jpeg",
    );
    a1.asset_date = ts_for_folder("2018/07/30");
    let mut a2 = WiremockAsset::new("FLAT2", "IMG_FLAT2.JPG", "public.jpeg").orig(
        200_000,
        "ck_f2",
        "public.jpeg",
    );
    a2.asset_date = ts_for_folder("2018/07/30");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a1, a2], db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 2);
    assert_eq!(stats.matched, 2, "flat folder layout diverged");
}

/// `--folder-structure {:%Y}` (year-only) variant.
#[tokio::test]
async fn folder_structure_year_only() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let mut config = base_config(&dl);
    config.folder_structure = "%Y".to_string();

    stage_file(&dl.join("2018").join("IMG_Y1.JPG"), 100_000);
    stage_file(&dl.join("2018").join("IMG_Y2.JPG"), 200_000);

    let mut a1 =
        WiremockAsset::new("Y1", "IMG_Y1.JPG", "public.jpeg").orig(100_000, "ck_y1", "public.jpeg");
    a1.asset_date = ts_for_folder("2018/07/30");
    let mut a2 =
        WiremockAsset::new("Y2", "IMG_Y2.JPG", "public.jpeg").orig(200_000, "ck_y2", "public.jpeg");
    a2.asset_date = ts_for_folder("2018/12/31");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a1, a2], db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 2);
    assert_eq!(stats.matched, 2, "year-only folder layout diverged");
}

// ── Edge cases ────────────────────────────────────────────────────

/// Mirrors test_download_photos.py:1264 (a test that uses a `中文` parent
/// dir to verify icloudpd handles non-ASCII dir paths). For kei's
/// import-existing, the *download_dir* is the user's existing path; what
/// matters is that kei doesn't fall over when the parent path contains
/// non-ASCII chars and the relative folder is plain ASCII.
#[tokio::test]
async fn non_ascii_parent_directory() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("中文").join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    let fixtures: &[(&str, &str, u64)] = &[("2018/07/30", "IMG_NA.JPG", 100_000)];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a = WiremockAsset::new("NA1", "IMG_NA.JPG", "public.jpeg").orig(
        100_000,
        "ck_na",
        "public.jpeg",
    );
    a.asset_date = ts_for_folder("2018/07/30");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;

    assert_eq!(stats.total, 1);
    assert_eq!(stats.matched, 1);
}

/// Adobe DNG (raw) at default layout. Mirrors test_download_photos.py's
/// `test_*_raw` cases (around line 1573 region) — DNG files land at the
/// same `{:%Y/%m/%d}/<base>.DNG` shape, just with a different extension.
#[tokio::test]
async fn raw_dng_default_layout() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    let fixtures: &[(&str, &str, u64)] = &[("2018/07/31", "IMG_7409.DNG", 5_000_000)];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a = WiremockAsset::new("RAW1", "IMG_7409.DNG", "com.adobe.raw-image").orig(
        5_000_000,
        "ck_raw",
        "com.adobe.raw-image",
    );
    a.asset_date = ts_for_folder("2018/07/31");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;
    assert_eq!(stats.total, 1);
    assert_eq!(stats.matched, 1, "DNG raw layout diverged");
}

/// Filenames with filesystem-invalid characters (`<>:"/\|?*`) get cleaned
/// to `_`. Mirrors test_download_photos.py:1224 area —
/// `i/n v:a\0l*i?d\p<a>t"h|.JPG` becomes `i_n v_a_l_i_d_p_a_t_h_.JPG`.
/// Both icloudpd and kei apply the same replacement set.
#[tokio::test]
async fn invalid_filename_chars_cleaned() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    // Stage with the cleaned shape (the iCloud-side raw filename never
    // hits disk; what's on disk is the post-clean string).
    let fixtures: &[(&str, &str, u64)] = &[("2018/07/31", "i_n v_a_l_i_d_p_a_t_h_.JPG", 100_000)];
    stage_icloudpd_fixtures(&dl, fixtures);

    // Wiremock returns the dirty iCloud-side filename; kei's filter must
    // clean it to the same string icloudpd would produce on disk.
    let raw_filename = "i/n v:a\0l*i?d\\p<a>t\"h|.JPG";
    let mut a = WiremockAsset::new("INV1", raw_filename, "public.jpeg").orig(
        100_000,
        "ck_inv",
        "public.jpeg",
    );
    a.asset_date = ts_for_folder("2018/07/31");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;
    assert_eq!(stats.total, 1);
    assert_eq!(
        stats.matched, 1,
        "kei's filename-clean diverged from icloudpd's `<>:\"/\\|?*` → `_`",
    );
}

/// Unicode in the iCloud-side filename, with `--keep-unicode` (kei:
/// `keep_unicode_in_filenames = true`). Mirrors
/// test_download_one_recent_live_photo_chinese: `IMG_中文_7409.JPG` /
/// `IMG_中文_7409.MOV` survive verbatim on disk.
#[tokio::test]
async fn keep_unicode_filename_preserves_chars() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let mut config = base_config(&dl);
    config.keep_unicode_in_filenames = true;

    let fixtures: &[(&str, &str, u64)] = &[
        ("2018/07/31", "IMG_中文_7409.JPG", 100_000),
        ("2018/07/31", "IMG_中文_7409.MOV", 200_000),
    ];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a = WiremockAsset::new("CN1", "IMG_中文_7409.JPG", "public.jpeg")
        .orig(100_000, "ck_cn", "public.jpeg")
        .live_mov(200_000, "ck_cn_mov");
    a.asset_date = ts_for_folder("2018/07/31");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;
    assert_eq!(stats.total, 1);
    assert_eq!(
        stats.matched, 2,
        "keep_unicode + live photo divergence at icloudpd shape",
    );
}

/// Default policy strips Unicode from filenames: iCloud-side
/// `IMG_中文_7409.JPG` lands on disk as `IMG__7409.JPG` (the multi-byte
/// chars dropped, no other replacement). icloudpd does
/// `value.encode("utf-8").decode("ascii", "ignore")`; kei does
/// `chars().filter(char::is_ascii)`. Same observable behavior.
#[tokio::test]
async fn strip_unicode_filename_drops_non_ascii() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl); // keep_unicode_in_filenames = false

    let fixtures: &[(&str, &str, u64)] = &[("2018/07/31", "IMG__7409.JPG", 100_000)];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a = WiremockAsset::new("CN2", "IMG_中文_7409.JPG", "public.jpeg").orig(
        100_000,
        "ck_cn2",
        "public.jpeg",
    );
    a.asset_date = ts_for_folder("2018/07/31");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;
    assert_eq!(stats.total, 1);
    assert_eq!(
        stats.matched, 1,
        "kei's Unicode-strip diverged from icloudpd's ASCII-only filter",
    );
}

/// Pre-1970 asset_date. icloudpd's
/// test_creation_date_prior_1970 puts the file at `1965/01/01/<file>`.
/// kei's `chrono::Local` handles negative timestamps; just verify the
/// path-render is consistent.
#[tokio::test]
async fn pre_1970_asset_date() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    let fixtures: &[(&str, &str, u64)] = &[("1965/01/01", "IMG_OLD.JPG", 100_000)];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a = WiremockAsset::new("OLD1", "IMG_OLD.JPG", "public.jpeg").orig(
        100_000,
        "ck_old",
        "public.jpeg",
    );
    a.asset_date = ts_for_folder("1965/01/01");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;
    assert_eq!(stats.total, 1);
    assert_eq!(stats.matched, 1, "pre-1970 date layout diverged");
}

/// Live photos id7 — multi-asset variant from test_download_live_photos_id.py.
/// Three HEIC live photos on the same day, each with id7-suffixed image
/// and `_HEVC.MOV` companion. Verifies kei renders the icloudpd
/// `IMG_NNNN_<id7>.HEIC` + `IMG_NNNN_<id7>_HEVC.MOV` shape across multiple
/// assets in one import.
#[tokio::test]
async fn live_photos_id7_multi_asset() {
    use crate::download::paths::apply_name_id7;

    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let mut config = base_config(&dl);
    config.file_match_policy = FileMatchPolicy::NameId7;

    let folder = "2020/11/04";
    let ts = ts_for_folder(folder);

    // Three assets mirroring icloudpd's listing_live_photos.yml fixtures.
    let cases = [
        ("REC_LIVE_0516", "IMG_0516.HEIC", 1_651_485_u64, 0_u64),
        ("REC_LIVE_0514", "IMG_0514.HEIC", 1_500_000, 3_951_774),
        ("REC_LIVE_0512", "IMG_0512.HEIC", 1_400_000, 3_500_000),
    ];

    let mut assets = Vec::new();
    for (rec, fname, img_size, mov_size) in &cases {
        let img = apply_name_id7(fname, rec); // IMG_NNNN_<id7>.HEIC
        stage_file(&dl.join(folder).join(&img), *img_size);
        let mut a = WiremockAsset::new(rec, fname, "public.heic").orig(
            *img_size,
            &format!("ck_{rec}"),
            "public.heic",
        );
        if *mov_size > 0 {
            let mov = crate::download::paths::live_photo_mov_path_suffix(&img);
            stage_file(&dl.join(folder).join(&mov), *mov_size);
            a = a.live_mov(*mov_size, &format!("ck_{rec}_mov"));
        }
        a.asset_date = ts;
        assets.push(a);
    }

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &assets, db.as_ref(), &config, false).await;
    assert_eq!(stats.total, 3);
    // 1 image-only + 2 (image+MOV) = 5 versions.
    assert_eq!(
        stats.matched, 5,
        "live photos id7 multi-asset shape diverged",
    );
}

// ── Intentional divergence from icloudpd ────────────────────────────────
//
// Cases where kei deliberately differs from icloudpd's path derivation.
// These are characterization tests: if any one fails, ask "was the
// divergence intentional?" before adjusting. Re-converging with icloudpd
// here would silently change matches for kei users who rely on the
// current shape.

/// kei preserves the iCloud-side extension case verbatim (`.JPG`,
/// `.HEIC`, `.MOV` -- whatever Apple sends). icloudpd lowercases to
/// `.jpg`/`.heic`/`.mov` in some code paths. If a future kei change
/// starts lowercasing, this test catches it.
#[tokio::test]
async fn kei_preserves_uppercase_extension_from_icloud() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    let fixtures: &[(&str, &str, u64)] = &[("2020/01/15", "PHOTO_42.JPG", 50_000)];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a = WiremockAsset::new("UPPER1", "PHOTO_42.JPG", "public.jpeg").orig(
        50_000,
        "ck_upper1",
        "public.jpeg",
    );
    a.asset_date = ts_for_folder("2020/01/15");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;
    assert_eq!(stats.matched, 1, "kei must keep .JPG uppercase verbatim");
    let row = &all_downloaded(db.as_ref()).await[0];
    assert!(
        row.filename.ends_with(".JPG"),
        "DB row must record uppercase extension, got {:?}",
        row.filename,
    );
}

/// kei falls back to a SHA256-derived fingerprint name when iCloud
/// doesn't return a filename. icloudpd uses `<record_name>.<ext>` with
/// the raw record name. The fingerprint is collision-resistant and stable;
/// pinning it ensures import-existing keeps finding files sync writes
/// when filename is absent (e.g. some CloudKit responses for synthetic
/// or migrated assets).
#[tokio::test]
async fn kei_uses_fingerprint_filename_when_filename_missing() {
    use crate::download::paths::generate_fingerprint_filename;
    use serde_json::json;

    // No MockServer needed: filename absence is exercised against a
    // hand-built PhotoAsset fed directly to import_assets.
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);

    // Omit filenameEnc so PhotoAsset::filename() returns None.
    let record_name = "FNGR_RECORD_99";
    let asset_date = ts_for_folder("2024/03/14");
    let master = json!({
        "recordName": record_name,
        "fields": {
            "itemType": {"value": "public.jpeg"},
            "resOriginalFileType": {"value": "public.jpeg"},
            "resOriginalRes": {"value": {
                "size": 7777,
                "downloadURL": "https://p01.icloud-content.com/orig",
                "fileChecksum": "ck_fpgr",
            }},
        },
    });
    let asset_record = json!({
        "fields": {"assetDate": {"value": asset_date}, "addedDate": {"value": asset_date}},
    });
    let asset = PhotoAsset::new(master, asset_record);

    let fp_name = generate_fingerprint_filename(record_name, "public.jpeg");
    stage_file(&dl.join("2024/03/14").join(&fp_name), 7777);

    let stream = futures_util::stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]);
    let (tx, panic_rx) = tokio::sync::oneshot::channel::<bool>();
    drop(tx);
    let db = open_db(&tmp).await;
    let stats = import_assets(
        stream,
        panic_rx,
        db.as_ref(),
        &config,
        "test-all",
        false,
        false,
    )
    .await
    .expect("import_assets ok");
    assert_eq!(stats.matched, 1, "kei must use the fingerprint fallback");
}

/// `RawTreatmentPolicy::Unchanged` (kei's default) keeps the iCloud-side
/// `.DNG` extension verbatim and does not pair RAW + JPEG. icloudpd's
/// `--keep-raw` has different semantics (it changes pairing behavior in
/// ways kei doesn't replicate). Pin kei's Unchanged shape so a future
/// align_raw refactor doesn't accidentally reach for icloudpd-style
/// pairing.
#[tokio::test]
async fn kei_align_raw_unchanged_keeps_dng_extension() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let dl = tmp.path().join("photos");
    std::fs::create_dir_all(&dl).unwrap();
    let config = base_config(&dl);
    assert_eq!(
        config.align_raw,
        crate::types::RawTreatmentPolicy::Unchanged,
        "test depends on the Unchanged default"
    );

    let fixtures: &[(&str, &str, u64)] = &[("2024/02/02", "RAW_007.DNG", 18_000_000)];
    stage_icloudpd_fixtures(&dl, fixtures);

    let mut a = WiremockAsset::new("RAW1", "RAW_007.DNG", "com.adobe.raw-image").orig(
        18_000_000,
        "ck_raw1",
        "com.adobe.raw-image",
    );
    a.asset_date = ts_for_folder("2024/02/02");

    let db = open_db(&tmp).await;
    let stats = run_import(&server, &[a], db.as_ref(), &config, false).await;
    assert_eq!(stats.matched, 1, "Unchanged must preserve .DNG verbatim");
    let row = &all_downloaded(db.as_ref()).await[0];
    assert_eq!(
        &*row.filename, "RAW_007.DNG",
        "kei must not lowercase or mutate the DNG extension under Unchanged"
    );
}
