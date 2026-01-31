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

    // Support both Python icloudpd's `{:%Y}` syntax and plain `%Y` for
    // backwards compatibility with existing user configurations.
    let year = format!("{:04}", created_date.year());
    let month = format!("{:02}", created_date.month());
    let day = format!("{:02}", created_date.day());
    let hour = format!("{:02}", created_date.hour());
    let minute = format!("{:02}", created_date.minute());
    let second = format!("{:02}", created_date.second());

    let date_path = folder_structure
        // Python-style {:%Y} format
        .replace("{:%Y}", &year)
        .replace("{:%m}", &month)
        .replace("{:%d}", &day)
        .replace("{:%H}", &hour)
        .replace("{:%M}", &minute)
        .replace("{:%S}", &second)
        // Plain strftime %Y format
        .replace("%Y", &year)
        .replace("%m", &month)
        .replace("%d", &day)
        .replace("%H", &hour)
        .replace("%M", &minute)
        .replace("%S", &second);

    directory.join(&date_path).join(&clean)
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
#[allow(dead_code)]
pub fn remove_unicode_chars(filename: &str) -> String {
    filename.chars().filter(|c| c.is_ascii()).collect()
}

/// Add a size-based deduplication suffix to a filename.
///
/// For example, `"photo.jpg"` with size `12345` becomes `"photo-12345.jpg"`.
/// If the filename has no extension, the suffix is simply appended.
#[allow(dead_code)]
pub fn add_dedup_suffix(path: &str, size: u64) -> String {
    match path.rfind('.') {
        Some(dot_pos) => {
            let (stem, ext) = path.split_at(dot_pos);
            format!("{}-{}{}", stem, size, ext)
        }
        None => format!("{}-{}", path, size),
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
    fn test_add_dedup_suffix() {
        assert_eq!(add_dedup_suffix("photo.jpg", 12345), "photo-12345.jpg");
        assert_eq!(add_dedup_suffix("photo", 100), "photo-100");
        assert_eq!(
            add_dedup_suffix("my.photo.png", 999),
            "my.photo-999.png"
        );
    }

}
