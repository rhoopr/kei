use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Local, Timelike};

/// Build the local download path for a photo asset.
///
/// `folder_structure` is a date format string such as `"{:%Y/%m/%d}"`. The
/// special value `"none"` (case-insensitive) disables date-based folders.
pub fn local_download_path(
    directory: &Path,
    folder_structure: &str,
    created_date: &DateTime<Local>,
    filename: &str,
) -> PathBuf {
    let clean = clean_filename(filename);

    if folder_structure.eq_ignore_ascii_case("none") {
        return directory.join(&clean);
    }

    // Support both Python icloudpd's `{:%Y/%m/%d}` syntax and plain `%Y/%m/%d`
    // for backwards compatibility with existing user configurations.
    let year = format!("{:04}", created_date.year());
    let month = format!("{:02}", created_date.month());
    let day = format!("{:02}", created_date.day());
    let hour = format!("{:02}", created_date.hour());
    let minute = format!("{:02}", created_date.minute());
    let second = format!("{:02}", created_date.second());

    // Extract format from Python-style {:%Y/%m/%d} wrapper if present
    let format_str = if folder_structure.starts_with("{:") && folder_structure.ends_with('}') {
        &folder_structure[2..folder_structure.len() - 1]
    } else {
        folder_structure
    };

    let date_path = format_str
        .replace("%Y", &year)
        .replace("%m", &month)
        .replace("%d", &day)
        .replace("%H", &hour)
        .replace("%M", &minute)
        .replace("%S", &second);

    // Split on "/" and join as path components to handle cross-platform paths.
    // This converts "{:%Y/%m/%d}" format like "2025/01/15" into proper PathBuf.
    let mut path = directory.to_path_buf();
    for component in date_path.split('/') {
        if !component.is_empty() {
            path = path.join(component);
        }
    }
    path.join(&clean)
}

/// Clean a filename by removing characters that are invalid on common
/// filesystems: `/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`, `|`.
pub fn clean_filename(filename: &str) -> String {
    filename
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect()
}

/// Remove non-ASCII (unicode) characters from a filename, keeping only
/// ASCII characters.
#[allow(dead_code)] // for --keep-unicode-in-filenames (parsed but not yet wired)
pub fn remove_unicode_chars(filename: &str) -> String {
    filename.chars().filter(|c| c.is_ascii()).collect()
}

/// Add a size-based deduplication suffix to a filename.
///
/// For example, `"photo.jpg"` with size `12345` becomes `"photo-12345.jpg"`.
/// If the filename has no extension, the suffix is simply appended.
#[allow(dead_code)] // for --file-match-policy (parsed but not yet wired)
pub fn add_dedup_suffix(path: &str, size: u64) -> String {
    insert_suffix(path, &size.to_string())
}

/// Add a string suffix before the file extension.
///
/// For example, `"photo.jpg"` with suffix `"abc"` becomes `"photo-abc.jpg"`.
pub fn insert_suffix(path: &str, suffix: &str) -> String {
    match path.rfind('.') {
        Some(dot_pos) => {
            let (stem, ext) = path.split_at(dot_pos);
            format!("{}-{}{}", stem, suffix, ext)
        }
        None => format!("{}-{}", path, suffix),
    }
}

/// Generate a live photo MOV filename using the "suffix" policy.
///
/// For HEIC files: `photo.HEIC` → `photo_HEVC.MOV`
/// For other files: `photo.JPG` → `photo.MOV`
pub fn live_photo_mov_path_suffix(filename: &str) -> String {
    match filename.rfind('.') {
        Some(dot) => {
            let (stem, ext) = filename.split_at(dot);
            let ext_lower = ext[1..].to_ascii_lowercase();
            if ext_lower == "heic" {
                format!("{}_HEVC.MOV", stem)
            } else {
                format!("{}.MOV", stem)
            }
        }
        None => format!("{}.MOV", filename),
    }
}

/// Generate a live photo MOV filename using the "original" policy.
///
/// Simply replaces the extension with `.MOV`: `photo.HEIC` → `photo.MOV`
pub fn live_photo_mov_path_original(filename: &str) -> String {
    match filename.rfind('.') {
        Some(dot) => {
            let (stem, _) = filename.split_at(dot);
            format!("{}.MOV", stem)
        }
        None => format!("{}.MOV", filename),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_filename() {
        assert_eq!(clean_filename("photo:1.jpg"), "photo1.jpg");
        assert_eq!(clean_filename("a/b\\c*d?e\"f<g>h|i"), "abcdefghi");
        assert_eq!(clean_filename("normal.jpg"), "normal.jpg");
    }

    #[test]
    fn test_remove_unicode_chars() {
        assert_eq!(remove_unicode_chars("hello"), "hello");
        assert_eq!(remove_unicode_chars("héllo wörld"), "hllo wrld");
        assert_eq!(remove_unicode_chars("日本語.jpg"), ".jpg");
    }

    #[test]
    fn test_live_photo_mov_path_suffix_heic() {
        assert_eq!(
            live_photo_mov_path_suffix("IMG_1234.HEIC"),
            "IMG_1234_HEVC.MOV"
        );
        assert_eq!(live_photo_mov_path_suffix("photo.heic"), "photo_HEVC.MOV");
    }

    #[test]
    fn test_live_photo_mov_path_suffix_non_heic() {
        assert_eq!(live_photo_mov_path_suffix("IMG_1234.JPG"), "IMG_1234.MOV");
        assert_eq!(live_photo_mov_path_suffix("photo.jpg"), "photo.MOV");
        assert_eq!(live_photo_mov_path_suffix("photo.png"), "photo.MOV");
    }

    #[test]
    fn test_live_photo_mov_path_suffix_no_extension() {
        assert_eq!(live_photo_mov_path_suffix("photo"), "photo.MOV");
    }

    #[test]
    fn test_live_photo_mov_path_original() {
        assert_eq!(
            live_photo_mov_path_original("IMG_1234.HEIC"),
            "IMG_1234.MOV"
        );
        assert_eq!(live_photo_mov_path_original("photo.JPG"), "photo.MOV");
        assert_eq!(live_photo_mov_path_original("photo"), "photo.MOV");
    }

    #[test]
    fn test_add_dedup_suffix() {
        assert_eq!(add_dedup_suffix("photo.jpg", 12345), "photo-12345.jpg");
        assert_eq!(add_dedup_suffix("photo", 100), "photo-100");
        assert_eq!(add_dedup_suffix("my.photo.png", 999), "my.photo-999.png");
    }

    #[test]
    fn test_insert_suffix() {
        assert_eq!(
            insert_suffix("IMG_0001.MOV", "ASSET_ID"),
            "IMG_0001-ASSET_ID.MOV"
        );
        assert_eq!(insert_suffix("photo", "123"), "photo-123");
        assert_eq!(insert_suffix("a.b.mov", "id"), "a.b-id.mov");
    }
}
