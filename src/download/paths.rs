use std::fmt::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
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

    // Extract format from Python-style {:%Y/%m/%d} wrapper if present
    let format_str = if folder_structure.starts_with("{:") && folder_structure.ends_with('}') {
        &folder_structure[2..folder_structure.len() - 1]
    } else {
        folder_structure
    };

    // Build date path in a single allocation by scanning for % tokens
    // and replacing them inline, avoiding 6 intermediate String allocations.
    let date_path = expand_date_format(format_str, created_date);

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

/// Expand date format tokens (%Y, %m, %d, %H, %M, %S) in a single pass.
///
/// Avoids the 6 intermediate String allocations from chained `.replace()` calls.
fn expand_date_format(format_str: &str, date: &DateTime<Local>) -> String {
    let mut result = String::with_capacity(format_str.len() + 8);
    let mut chars = format_str.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.peek() {
                Some('Y') => {
                    chars.next();
                    let _ = write!(result, "{:04}", date.year());
                }
                Some('m') => {
                    chars.next();
                    let _ = write!(result, "{:02}", date.month());
                }
                Some('d') => {
                    chars.next();
                    let _ = write!(result, "{:02}", date.day());
                }
                Some('H') => {
                    chars.next();
                    let _ = write!(result, "{:02}", date.hour());
                }
                Some('M') => {
                    chars.next();
                    let _ = write!(result, "{:02}", date.minute());
                }
                Some('S') => {
                    chars.next();
                    let _ = write!(result, "{:02}", date.second());
                }
                _ => result.push(c), // Unknown token, keep the %
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Clean a filename by removing characters that are invalid on common
/// filesystems: `/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`, `|`.
pub fn clean_filename(filename: &str) -> String {
    filename
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect()
}

/// Sanitize a path component (e.g. album name) to prevent path traversal
/// and invalid directory names.
///
/// - Strips leading/trailing dots and spaces
/// - Replaces `..` sequences with `_`
/// - Removes filesystem-invalid characters via `clean_filename()`
/// - Prefixes Windows reserved names (CON, NUL, PRN, etc.) with `_`
pub fn sanitize_path_component(name: &str) -> String {
    // First clean invalid filesystem characters
    let cleaned = clean_filename(name);

    // Replace ".." sequences to prevent directory traversal
    let no_traversal = cleaned.replace("..", "_");

    // Strip leading/trailing dots and spaces
    let trimmed = no_traversal.trim_matches(|c: char| c == '.' || c == ' ');
    if trimmed.is_empty() {
        return "_".to_string();
    }

    // Check for Windows reserved names (case-insensitive)
    let upper = trimmed.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    if matches!(
        base,
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return format!("_{}", trimmed);
    }

    trimmed.to_string()
}

/// Remove non-ASCII (unicode) characters from a filename, keeping only
/// ASCII characters.
pub fn remove_unicode_chars(filename: &str) -> String {
    filename.chars().filter(|c| c.is_ascii()).collect()
}

/// Add a size-based deduplication suffix to a filename.
///
/// For example, `"photo.jpg"` with size `12345` becomes `"photo-12345.jpg"`.
/// If the filename has no extension, the suffix is simply appended.
///
/// Formats the size directly into the result string, avoiding an intermediate
/// `size.to_string()` allocation.
pub fn add_dedup_suffix(path: &str, size: u64) -> String {
    match path.rfind('.') {
        Some(dot_pos) => {
            let (stem, ext) = path.split_at(dot_pos);
            // Pre-allocate: stem + "-" + max 20 digits for u64 + ext
            let mut result = String::with_capacity(stem.len() + 1 + 20 + ext.len());
            result.push_str(stem);
            result.push('-');
            let _ = write!(result, "{}", size);
            result.push_str(ext);
            result
        }
        None => {
            let mut result = String::with_capacity(path.len() + 1 + 20);
            result.push_str(path);
            result.push('-');
            let _ = write!(result, "{}", size);
            result
        }
    }
}

/// Add a string suffix before the file extension.
///
/// For example, `"photo.jpg"` with suffix `"abc"` becomes `"photo-abc.jpg"`.
pub fn insert_suffix(path: &str, suffix: &str) -> String {
    match path.rfind('.') {
        Some(dot_pos) => {
            let (stem, ext) = path.split_at(dot_pos);
            // Pre-allocate exact size needed
            let mut result = String::with_capacity(stem.len() + 1 + suffix.len() + ext.len());
            result.push_str(stem);
            result.push('-');
            result.push_str(suffix);
            result.push_str(ext);
            result
        }
        None => {
            let mut result = String::with_capacity(path.len() + 1 + suffix.len());
            result.push_str(path);
            result.push('-');
            result.push_str(suffix);
            result
        }
    }
}

/// Map UTI asset_type strings to standardized uppercase file extensions.
///
/// Matches Python icloudpd's `ITEM_TYPE_EXTENSIONS` mapping.
const ITEM_TYPE_EXTENSIONS: &[(&str, &str)] = &[
    ("public.heic", "HEIC"),
    ("public.heif", "HEIF"),
    ("public.jpeg", "JPG"),
    ("public.png", "PNG"),
    ("com.apple.quicktime-movie", "MOV"),
    ("com.adobe.raw-image", "DNG"),
    ("com.canon.cr2-raw-image", "CR2"),
    ("com.canon.crw-raw-image", "CRW"),
    ("com.sony.arw-raw-image", "ARW"),
    ("com.fuji.raw-image", "RAF"),
    ("com.panasonic.rw2-raw-image", "RW2"),
    ("com.nikon.nrw-raw-image", "NRF"),
    ("com.pentax.raw-image", "PEF"),
    ("com.nikon.raw-image", "NEF"),
    ("com.olympus.raw-image", "ORF"),
    ("com.canon.cr3-raw-image", "CR3"),
    ("com.olympus.or-raw-image", "ORF"),
    ("org.webmproject.webp", "WEBP"),
];

/// Replace a filename's extension based on the UTI `asset_type` string.
///
/// If `asset_type` is found in `ITEM_TYPE_EXTENSIONS`, the filename's extension
/// is replaced with the mapped uppercase extension. Otherwise the original
/// filename is returned unchanged.
pub fn map_filename_extension(filename: &str, asset_type: &str) -> String {
    let ext = item_type_extension(asset_type);
    if ext == "unknown" {
        return filename.to_string();
    }
    match filename.rfind('.') {
        Some(dot) => format!("{}.{}", &filename[..dot], ext),
        None => format!("{}.{}", filename, ext),
    }
}

/// Compute the first 7 characters of the base64-encoded asset ID.
///
/// Used by the `name-id7` file match policy to create unique filenames.
fn base64_id7(id: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(id.as_bytes());
    encoded.chars().take(7).collect()
}

/// Apply the `name-id7` policy: insert the first 7 chars of the base64-encoded
/// asset ID as a suffix before the file extension, using underscore separator.
///
/// Matches Python's `add_suffix_to_filename(f"_{id_suffix}", filename)`.
pub fn apply_name_id7(filename: &str, id: &str) -> String {
    let suffix = base64_id7(id);
    match filename.rfind('.') {
        Some(dot) => {
            let (stem, ext) = filename.split_at(dot);
            format!("{}_{}{}", stem, suffix, ext)
        }
        None => format!("{}_{}", filename, suffix),
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

/// Look up the file extension for a UTI asset type string.
///
/// Returns the uppercase extension (e.g. `"JPG"`) or `"unknown"` if not mapped.
pub fn item_type_extension(asset_type: &str) -> &'static str {
    ITEM_TYPE_EXTENSIONS
        .iter()
        .find(|(key, _)| *key == asset_type)
        .map(|(_, ext)| *ext)
        .unwrap_or("unknown")
}

/// Generate a fallback filename from the asset ID when `filenameEnc` is absent.
///
/// Replaces non-alphanumeric characters with underscores and truncates to 12 chars,
/// then appends the extension derived from the asset's UTI type.
/// Matches Python's `generate_fingerprint_filename()`.
pub fn generate_fingerprint_filename(asset_id: &str, asset_type: &str) -> String {
    let fingerprint: String = asset_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(12)
        .collect();
    let ext = item_type_extension(asset_type);
    format!("{}.{}", fingerprint, ext)
}

/// Normalize AM/PM whitespace variants to a canonical no-space form.
///
/// macOS uses various whitespace characters before AM/PM:
/// - Regular space (U+0020): `1.40.01 PM`
/// - Narrow no-break space (U+202F): `1.40.01\u{202F}PM`
/// - No space: `1.40.01PM`
///
/// This function strips any of these to produce a consistent `1.40.01PM` form,
/// enabling matching between files created with different locale settings.
pub fn normalize_ampm(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        // Check if current char is whitespace before "AM" or "PM"
        if i + 2 < len
            && (chars[i] == ' ' || chars[i] == '\u{202F}' || chars[i] == '\u{00A0}')
            && ((chars[i + 1] == 'A' || chars[i + 1] == 'a')
                && (chars[i + 2] == 'M' || chars[i + 2] == 'm'))
            || (i + 2 < len
                && (chars[i] == ' ' || chars[i] == '\u{202F}' || chars[i] == '\u{00A0}')
                && ((chars[i + 1] == 'P' || chars[i + 1] == 'p')
                    && (chars[i + 2] == 'M' || chars[i + 2] == 'm')))
        {
            // Skip the whitespace, keep AM/PM
            i += 1;
            continue;
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Find a file on disk that differs only in AM/PM whitespace from the expected path.
///
/// When the expected file doesn't exist, this checks sibling files in the same
/// directory for an AM/PM whitespace variant (e.g., `1.40.01 PM.PNG` vs
/// `1.40.01\u{202F}PM.PNG` vs `1.40.01PM.PNG`).
///
/// Returns the matching variant's full path, or `None` if no match is found.
pub fn find_ampm_variant(path: &Path) -> Option<PathBuf> {
    let filename = path.file_name()?.to_str()?;
    let normalized = normalize_ampm(filename);

    // Early exit: if normalizing doesn't change the name, there's no AM/PM to vary
    if normalized == filename {
        return None;
    }

    let parent = path.parent()?;
    let entries = std::fs::read_dir(parent).ok()?;

    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        if let Some(sibling) = entry_name.to_str() {
            if sibling == filename {
                continue; // Skip exact match (shouldn't exist, but be safe)
            }
            if normalize_ampm(sibling) == normalized {
                return Some(entry.path());
            }
        }
    }

    None
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

    #[test]
    fn test_map_filename_extension_known_types() {
        assert_eq!(
            map_filename_extension("IMG_0001.jpeg", "public.jpeg"),
            "IMG_0001.JPG"
        );
        assert_eq!(
            map_filename_extension("photo.heic", "public.heic"),
            "photo.HEIC"
        );
        assert_eq!(
            map_filename_extension("video.mov", "com.apple.quicktime-movie"),
            "video.MOV"
        );
        assert_eq!(
            map_filename_extension("raw.cr2", "com.canon.cr2-raw-image"),
            "raw.CR2"
        );
        assert_eq!(
            map_filename_extension("photo.png", "public.png"),
            "photo.PNG"
        );
    }

    #[test]
    fn test_map_filename_extension_webp() {
        assert_eq!(
            map_filename_extension("photo.webp", "org.webmproject.webp"),
            "photo.WEBP"
        );
    }

    #[test]
    fn test_map_filename_extension_unknown_type() {
        assert_eq!(
            map_filename_extension("photo.xyz", "com.unknown.type"),
            "photo.xyz"
        );
    }

    #[test]
    fn test_map_filename_extension_no_extension() {
        assert_eq!(map_filename_extension("photo", "public.jpeg"), "photo.JPG");
    }

    #[test]
    fn test_apply_name_id7() {
        let result = apply_name_id7("IMG_0001.JPG", "ABC123");
        // base64("ABC123") = "QUJDMTIz", first 7 = "QUJDMTI"
        assert_eq!(result, "IMG_0001_QUJDMTI.JPG");
    }

    #[test]
    fn test_apply_name_id7_no_extension() {
        let result = apply_name_id7("photo", "XYZ");
        // base64("XYZ") = "WFla", first 7 (only 4 available) = "WFla"
        assert_eq!(result, "photo_WFla");
    }

    #[test]
    fn test_base64_id7_length() {
        // Longer IDs should produce exactly 7 chars
        let result = base64_id7("AaBbCcDdEeFfGg/HhIiJj+KkLl");
        assert_eq!(result.len(), 7);
    }

    #[test]
    fn test_remove_unicode_strips_narrow_no_break_space() {
        // U+202F (NARROW NO-BREAK SPACE) is used before AM/PM in macOS screenshots
        let input = "Screenshot 2025-04-03 at 1.40.01\u{202F}PM.PNG";
        assert_eq!(
            remove_unicode_chars(input),
            "Screenshot 2025-04-03 at 1.40.01PM.PNG"
        );
    }

    #[test]
    fn test_insert_suffix_medium_thumb() {
        // Matches Python's VERSION_FILENAME_SUFFIX_LOOKUP behavior
        assert_eq!(
            insert_suffix("IMG_5526.JPG", "medium"),
            "IMG_5526-medium.JPG"
        );
        assert_eq!(insert_suffix("IMG_5526.JPG", "thumb"), "IMG_5526-thumb.JPG");
        assert_eq!(
            insert_suffix("IMG_5526_QUJDMTI.JPG", "medium"),
            "IMG_5526_QUJDMTI-medium.JPG"
        );
    }

    #[test]
    fn test_item_type_extension() {
        assert_eq!(item_type_extension("public.jpeg"), "JPG");
        assert_eq!(item_type_extension("public.heic"), "HEIC");
        assert_eq!(item_type_extension("com.apple.quicktime-movie"), "MOV");
        assert_eq!(item_type_extension("org.webmproject.webp"), "WEBP");
        assert_eq!(item_type_extension("unknown.type"), "unknown");
    }

    #[test]
    fn test_generate_fingerprint_filename() {
        // Matches Python: re.sub("[^0-9a-zA-Z]", "_", asset_id)[0:12]
        assert_eq!(
            generate_fingerprint_filename("CCPO9c3V/MTwWZJ9bw==", "public.jpeg"),
            "CCPO9c3V_MTw.JPG"
        );
        assert_eq!(
            generate_fingerprint_filename("ABC", "public.heic"),
            "ABC.HEIC"
        );
        assert_eq!(
            generate_fingerprint_filename("a/b+c=d!e@f#g$h%i", "public.png"),
            "a_b_c_d_e_f_.PNG"
        );
    }

    #[test]
    fn test_normalize_ampm_strips_space() {
        assert_eq!(
            normalize_ampm("Screenshot 2025-04-03 at 1.40.01 PM.PNG"),
            "Screenshot 2025-04-03 at 1.40.01PM.PNG"
        );
    }

    #[test]
    fn test_normalize_ampm_strips_narrow_no_break_space() {
        assert_eq!(
            normalize_ampm("Screenshot 2025-04-03 at 1.40.01\u{202F}PM.PNG"),
            "Screenshot 2025-04-03 at 1.40.01PM.PNG"
        );
    }

    #[test]
    fn test_normalize_ampm_no_change_without_ampm() {
        assert_eq!(normalize_ampm("photo.jpg"), "photo.jpg");
        assert_eq!(normalize_ampm("IMG_0001.HEIC"), "IMG_0001.HEIC");
    }

    #[test]
    fn test_normalize_ampm_already_no_space() {
        assert_eq!(
            normalize_ampm("Screenshot 2025-04-03 at 1.40.01PM.PNG"),
            "Screenshot 2025-04-03 at 1.40.01PM.PNG"
        );
    }

    #[test]
    fn test_normalize_ampm_am() {
        assert_eq!(
            normalize_ampm("Screenshot at 10.30.00 AM.PNG"),
            "Screenshot at 10.30.00AM.PNG"
        );
    }

    #[test]
    fn test_find_ampm_variant_finds_match() {
        use std::fs;
        let dir = std::env::temp_dir().join("claude").join("ampm_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Create a file with space before PM
        let existing = dir.join("Screenshot at 1.40.01 PM.PNG");
        fs::write(&existing, b"test").unwrap();

        // Look for the narrow-no-break-space variant
        let query = dir.join("Screenshot at 1.40.01\u{202F}PM.PNG");
        let found = find_ampm_variant(&query);
        assert!(found.is_some());
        assert_eq!(found.unwrap(), existing);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_ampm_variant_returns_none_for_non_ampm() {
        let path = Path::new("/tmp/claude/nonexistent/photo.jpg");
        assert!(find_ampm_variant(path).is_none());
    }

    #[test]
    fn test_sanitize_path_component_traversal() {
        // clean_filename removes "/" so "../etc/passwd" → "..etcpasswd" → "_etcpasswd"
        assert_eq!(sanitize_path_component("../etc/passwd"), "_etcpasswd");
        assert_eq!(sanitize_path_component(".."), "_");
        // "foo/../bar" → clean removes "/" → "foo..bar" → replace ".." → "foo_bar"
        assert_eq!(sanitize_path_component("foo/../bar"), "foo_bar");
    }

    #[test]
    fn test_sanitize_path_component_dots_and_spaces() {
        // "...hidden..." → clean → "...hidden..." → replace ".." → "_.hidden_." → trim dots → "_.hidden_"
        assert_eq!(sanitize_path_component("...hidden..."), "_.hidden_");
        assert_eq!(sanitize_path_component("  spaced  "), "spaced");
        assert_eq!(sanitize_path_component(".dotfile"), "dotfile");
    }

    #[test]
    fn test_sanitize_path_component_reserved_names() {
        assert_eq!(sanitize_path_component("CON"), "_CON");
        assert_eq!(sanitize_path_component("nul"), "_nul");
        assert_eq!(sanitize_path_component("PRN"), "_PRN");
        assert_eq!(sanitize_path_component("COM1"), "_COM1");
        assert_eq!(sanitize_path_component("LPT3"), "_LPT3");
    }

    #[test]
    fn test_sanitize_path_component_normal() {
        assert_eq!(sanitize_path_component("My Album"), "My Album");
        assert_eq!(sanitize_path_component("Vacation 2024"), "Vacation 2024");
    }

    #[test]
    fn test_sanitize_path_component_empty() {
        assert_eq!(sanitize_path_component(""), "_");
        assert_eq!(sanitize_path_component("..."), "_");
        assert_eq!(sanitize_path_component("   "), "_");
    }

    #[test]
    fn test_generate_fingerprint_filename_unknown_type() {
        assert_eq!(
            generate_fingerprint_filename("asset123", "some.unknown.type"),
            "asset123.unknown"
        );
    }
}
