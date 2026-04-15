use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine;
use chrono::{DateTime, Local};
use rustc_hash::FxHashMap;

/// Strip the legacy Python-style `{:%Y/%m/%d}` wrapper, returning the inner
/// format string. Returns the input unchanged if the wrapper is absent.
fn strip_python_wrapper(folder_structure: &str) -> &str {
    if folder_structure.starts_with("{:") && folder_structure.ends_with('}') {
        &folder_structure[2..folder_structure.len() - 1]
    } else {
        folder_structure
    }
}

/// Expand the `{album}` token in a folder structure format string.
///
/// Strips the Python-style wrapper, sanitizes the album name as a path
/// component, escapes `%` for chrono strftime, and replaces `{album}`.
/// Returns the original `folder_structure` (wrapper-stripped) unchanged if
/// `{album}` is absent.
pub(crate) fn expand_album_token(folder_structure: &str, album_name: Option<&str>) -> String {
    let format_str = strip_python_wrapper(folder_structure);
    if !format_str.contains("{album}") {
        return format_str.to_string();
    }
    let safe_name = album_name
        .filter(|n| !n.is_empty())
        .map(|n| sanitize_path_component(n).replace('%', "%%"))
        .unwrap_or_default();
    format_str.replace("{album}", &safe_name)
}

/// Build the date-based parent directory for a photo asset (without filename).
///
/// `folder_structure` is a strftime format string such as `"%Y/%m/%d"`. The
/// legacy Python-style `"{:%Y/%m/%d}"` wrapper is also accepted. The custom
/// `{album}` token is expanded to the album name before strftime processing.
/// The special value `"none"` (case-insensitive) disables date-based folders.
pub(crate) fn local_download_dir(
    directory: &Path,
    folder_structure: &str,
    created_date: &DateTime<Local>,
    album_name: Option<&str>,
) -> PathBuf {
    if folder_structure.eq_ignore_ascii_case("none") {
        return directory.to_path_buf();
    }

    let expanded = expand_album_token(folder_structure, album_name);

    // Use chrono's strftime for full format token support (%Y, %m, %d, %B, etc.)
    let date_path = created_date.format(&expanded).to_string();

    // Split on "/" and join as path components to handle cross-platform paths.
    // Each component is sanitized to prevent path traversal (e.g. "../../etc").
    let mut path = directory.to_path_buf();
    for component in date_path.split('/') {
        if !component.is_empty() {
            path = path.join(sanitize_path_component(component));
        }
    }
    path
}

/// Build the local download path for a photo asset.
///
/// `folder_structure` is a strftime format string such as `"%Y/%m/%d"`. The
/// legacy Python-style `"{:%Y/%m/%d}"` wrapper is also accepted. The custom
/// `{album}` token is expanded to the album name before strftime processing.
/// The special value `"none"` (case-insensitive) disables date-based folders.
pub(crate) fn local_download_path(
    directory: &Path,
    folder_structure: &str,
    created_date: &DateTime<Local>,
    filename: &str,
    album_name: Option<&str>,
) -> PathBuf {
    local_download_dir(directory, folder_structure, created_date, album_name)
        .join(clean_filename(filename))
}

/// Maximum filename length in bytes for common filesystems (ext4, APFS, NTFS).
const MAX_FILENAME_BYTES: usize = 255;

/// Clean a filename by replacing characters that are invalid on common
/// filesystems (`/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`, `|`) and control
/// characters (including NUL) with `_`. Truncates filenames exceeding 255
/// bytes, preserving the file extension.
pub(crate) fn clean_filename(filename: &str) -> String {
    let cleaned: String = filename
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();

    if cleaned.len() <= MAX_FILENAME_BYTES {
        return cleaned;
    }

    // Preserve the extension (e.g. ".jpg") when truncating, but only if it
    // leaves room for at least one stem character.
    if let Some(dot) = cleaned.rfind('.') {
        let ext = &cleaned[dot..];
        if ext.len() < MAX_FILENAME_BYTES {
            let stem_end = cleaned[..dot].floor_char_boundary(MAX_FILENAME_BYTES - ext.len());
            return format!("{}{ext}", &cleaned[..stem_end]);
        }
    }

    cleaned[..cleaned.floor_char_boundary(MAX_FILENAME_BYTES)].to_string()
}

/// Sanitize a path component (e.g. album name) to prevent path traversal
/// and invalid directory names.
///
/// - Strips leading/trailing dots and spaces
/// - Replaces `..` sequences with `_`
/// - Replaces filesystem-invalid characters via `clean_filename()`
/// - Prefixes Windows reserved names (CON, NUL, PRN, etc.) with `_`
pub(crate) fn sanitize_path_component(name: &str) -> String {
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
    #[cfg(target_os = "windows")]
    {
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
    }

    trimmed.to_string()
}

/// Remove non-ASCII (unicode) characters from a filename, keeping only
/// ASCII characters.
pub(crate) fn remove_unicode_chars(filename: &str) -> String {
    filename.chars().filter(char::is_ascii).collect()
}

/// Add a size-based deduplication suffix to a filename.
///
/// For example, `"photo.jpg"` with size `12345` becomes `"photo-12345.jpg"`.
/// If the filename has no extension, the suffix is simply appended.
///
/// Formats the size directly into the result string, avoiding an intermediate
/// `size.to_string()` allocation.
pub(crate) fn add_dedup_suffix(path: &str, size: u64) -> String {
    if let Some(dot_pos) = path.rfind('.') {
        let (stem, ext) = path.split_at(dot_pos);
        // Pre-allocate: stem + "-" + max 20 digits for u64 + ext
        let mut result = String::with_capacity(stem.len() + 1 + 20 + ext.len());
        result.push_str(stem);
        result.push('-');
        let _ = write!(result, "{size}");
        result.push_str(ext);
        result
    } else {
        let mut result = String::with_capacity(path.len() + 1 + 20);
        result.push_str(path);
        result.push('-');
        let _ = write!(result, "{size}");
        result
    }
}

/// Add a string suffix before the file extension.
///
/// For example, `"photo.jpg"` with suffix `"abc"` becomes `"photo-abc.jpg"`.
pub(crate) fn insert_suffix(path: &str, suffix: &str) -> String {
    if let Some(dot_pos) = path.rfind('.') {
        let (stem, ext) = path.split_at(dot_pos);
        // Pre-allocate exact size needed
        let mut result = String::with_capacity(stem.len() + 1 + suffix.len() + ext.len());
        result.push_str(stem);
        result.push('-');
        result.push_str(suffix);
        result.push_str(ext);
        result
    } else {
        let mut result = String::with_capacity(path.len() + 1 + suffix.len());
        result.push_str(path);
        result.push('-');
        result.push_str(suffix);
        result
    }
}

/// Map UTI `asset_type` strings to standardized uppercase file extensions.
///
/// Matches `icloudpd`'s `ITEM_TYPE_EXTENSIONS` mapping.
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
pub(crate) fn map_filename_extension(filename: &str, asset_type: &str) -> String {
    let ext = item_type_extension(asset_type);
    if ext == "unknown" {
        return filename.to_string();
    }
    match filename.rfind('.') {
        Some(dot) => format!("{}.{}", &filename[..dot], ext),
        None => format!("{filename}.{ext}"),
    }
}

/// Compute the first 7 characters of the base64-encoded asset ID.
///
/// Used by the `name-id7` file match policy to create unique filenames.
fn base64_id7(id: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(id.as_bytes());
    encoded[..encoded.len().min(7)].to_string()
}

/// Apply the `name-id7` policy: insert the first 7 chars of the base64-encoded
/// asset ID as a suffix before the file extension, using underscore separator.
///
/// Matches Python's `add_suffix_to_filename(f"_{id_suffix}", filename)`.
pub(crate) fn apply_name_id7(filename: &str, id: &str) -> String {
    let suffix = base64_id7(id);
    match filename.rfind('.') {
        Some(dot) => {
            let (stem, ext) = filename.split_at(dot);
            format!("{stem}_{suffix}{ext}")
        }
        None => format!("{filename}_{suffix}"),
    }
}

/// Generate a live photo MOV filename using the "suffix" policy.
///
/// For HEIC files: `photo.HEIC` → `photo_HEVC.MOV`
/// For other files: `photo.JPG` → `photo.MOV`
pub(crate) fn live_photo_mov_path_suffix(filename: &str) -> String {
    match filename.rfind('.') {
        Some(dot) => {
            let (stem, ext) = filename.split_at(dot);
            let ext_lower = ext[1..].to_ascii_lowercase();
            if ext_lower == "heic" {
                format!("{stem}_HEVC.MOV")
            } else {
                format!("{stem}.MOV")
            }
        }
        None => format!("{filename}.MOV"),
    }
}

/// Pre-built `HashMap` for O(1) asset type lookups instead of linear scan.
static ITEM_TYPE_MAP: LazyLock<FxHashMap<&'static str, &'static str>> =
    LazyLock::new(|| ITEM_TYPE_EXTENSIONS.iter().copied().collect());

/// Look up the file extension for a UTI asset type string.
///
/// Returns the uppercase extension (e.g. `"JPG"`) or `"unknown"` if not mapped.
pub(crate) fn item_type_extension(asset_type: &str) -> &'static str {
    ITEM_TYPE_MAP.get(asset_type).copied().unwrap_or("unknown")
}

/// Generate a fallback filename from the asset ID when `filenameEnc` is absent.
///
/// Uses a SHA-256 hash of the full asset ID (first 12 hex chars = 48 bits)
/// for collision resistance, instead of just taking the first 12 alphanumeric
/// characters which can collide when IDs differ only in non-alphanumeric
/// positions.
pub(crate) fn generate_fingerprint_filename(asset_id: &str, asset_type: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let hash = Sha256::digest(asset_id.as_bytes());
    let ext = item_type_extension(asset_type);
    let mut result = String::with_capacity(12 + 1 + ext.len());
    for &b in &hash[..6] {
        let _ = Write::write_fmt(&mut result, format_args!("{b:02x}"));
    }
    result.push('.');
    result.push_str(ext);
    result
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
pub(crate) fn normalize_ampm(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Check for whitespace characters that may precede AM/PM:
        // - Regular space (U+0020): 1 byte
        // - No-break space (U+00A0): 2 bytes (0xC2, 0xA0)
        // - Narrow no-break space (U+202F): 3 bytes (0xE2, 0x80, 0xAF)
        let ws_len = if bytes[i] == b' ' {
            1
        } else if i + 1 < len && bytes[i] == 0xC2 && bytes[i + 1] == 0xA0 {
            2 // U+00A0
        } else if i + 2 < len && bytes[i] == 0xE2 && bytes[i + 1] == 0x80 && bytes[i + 2] == 0xAF {
            3 // U+202F
        } else {
            0
        };

        if ws_len > 0 && i + ws_len + 1 < len {
            let next = bytes[i + ws_len].to_ascii_uppercase();
            let next2 = bytes[i + ws_len + 1].to_ascii_uppercase();
            if (next == b'A' || next == b'P') && next2 == b'M' {
                // Skip the whitespace, the AM/PM chars will be added on next iterations
                i += ws_len;
                continue;
            }
        }

        // Safe: we only skip known valid UTF-8 boundaries above
        if bytes[i] < 0x80 {
            result.push(bytes[i] as char);
            i += 1;
        } else {
            // Multi-byte UTF-8: decode the char and advance past it
            // Safe: i < len and bytes[i] >= 0x80 guarantees a multi-byte char starts here
            let ch = s[i..]
                .chars()
                .next()
                .expect("i < len guarantees a char exists");
            result.push(ch);
            i += ch.len_utf8();
        }
    }
    result
}

/// Read all entries in `dir`, returning a filename→size map.
///
/// This is the blocking I/O primitive used by `DirCache`. Extracted so it can
/// be called from both sync (`ensure_dir`) and async (`ensure_dir_async`) paths.
fn read_dir_entries(dir: &Path) -> FxHashMap<String, u64> {
    std::fs::read_dir(dir)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_str()?.to_string();
                    let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                    Some((name, size))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Cached directory listing that amortizes filesystem syscalls.
///
/// For each parent directory, a single `read_dir` populates a filename→size map.
/// All subsequent existence checks and size lookups for files in that directory
/// are served from the cache — eliminating per-file `stat()` calls that would
/// otherwise block the tokio runtime when called from an async task.
///
/// Async callers should pre-populate directories with `ensure_dir_async()` before
/// entering tight sync loops (e.g. `filter_asset_to_tasks`), so that the sync
/// lookup methods (`exists`, `file_size`, `find_ampm_variant`) never hit disk.
pub(crate) struct DirCache {
    dirs: FxHashMap<PathBuf, FxHashMap<String, u64>>,
}

impl DirCache {
    pub fn new() -> Self {
        Self {
            dirs: FxHashMap::default(),
        }
    }

    /// Invalidate all cached entries. Use after writing files to disk.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.dirs.clear();
    }

    /// Pre-populate the cache for `dir` on the blocking threadpool.
    ///
    /// Call this from async code before using the sync lookup methods, so that
    /// the subsequent `ensure_dir` calls are guaranteed cache-hits with no
    /// blocking I/O on the tokio worker thread.
    pub async fn ensure_dir_async(&mut self, dir: &Path) {
        if self.dirs.contains_key(dir) {
            return;
        }
        let dir_buf = dir.to_path_buf();
        let entries = tokio::task::spawn_blocking({
            let d = dir_buf.clone();
            move || read_dir_entries(&d)
        })
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(dir = %dir_buf.display(), error = %e, "Failed to read directory entries");
            FxHashMap::default()
        });
        self.dirs.insert(dir_buf, entries);
    }

    /// Check whether `path` exists on disk, using cached directory listings.
    pub fn exists(&mut self, path: &Path) -> bool {
        let Some(filename) = path.file_name().and_then(|f| f.to_str()) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        self.ensure_dir(parent).contains_key(filename)
    }

    /// Return the file size for `path` if it exists, using cached directory listings.
    /// Avoids a separate `std::fs::metadata()` call.
    pub fn file_size(&mut self, path: &Path) -> Option<u64> {
        let filename = path.file_name()?.to_str()?;
        let parent = path.parent()?;
        self.ensure_dir(parent).get(filename).copied()
    }

    /// Find a file on disk that differs only in AM/PM whitespace from `path`.
    ///
    /// Checks sibling files in the same directory for an AM/PM whitespace variant
    /// (e.g., `1.40.01 PM.PNG` vs `1.40.01\u{202F}PM.PNG` vs `1.40.01PM.PNG`).
    pub fn find_ampm_variant(&mut self, path: &Path) -> Option<PathBuf> {
        let filename = path.file_name()?.to_str()?;
        let normalized = normalize_ampm(filename);

        // Early exit: if normalizing doesn't change the name, there's no AM/PM to vary
        if normalized == filename {
            return None;
        }

        let parent = path.parent()?;
        let entries = self.ensure_dir(parent);

        for sibling in entries.keys() {
            if sibling == filename {
                continue;
            }
            if normalize_ampm(sibling) == normalized {
                return Some(parent.join(sibling.as_str()));
            }
        }

        None
    }

    /// Populate the cache for `dir` if not already present (blocking I/O).
    ///
    /// In async contexts, prefer `ensure_dir_async()` to avoid blocking the
    /// tokio worker thread — especially on slow or network-attached storage.
    fn ensure_dir(&mut self, dir: &Path) -> &FxHashMap<String, u64> {
        // Fast path: two lookups but zero allocation on cache hit.
        // get() would be one lookup, but its returned reference borrows
        // self.dirs immutably, which the borrow checker cannot release
        // before the mutable entry() call below.
        if self.dirs.contains_key(dir) {
            return &self.dirs[dir];
        }
        self.dirs
            .entry(dir.to_path_buf())
            .or_insert_with(|| read_dir_entries(dir))
    }
}

/// Generate a live photo MOV filename using the "original" policy.
///
/// Simply replaces the extension with `.MOV`: `photo.HEIC` → `photo.MOV`
pub(crate) fn live_photo_mov_path_original(filename: &str) -> String {
    match filename.rfind('.') {
        Some(dot) => {
            let (stem, _) = filename.split_at(dot);
            format!("{stem}.MOV")
        }
        None => format!("{filename}.MOV"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_clean_filename() {
        assert_eq!(clean_filename("photo:1.jpg"), "photo_1.jpg");
        assert_eq!(clean_filename("a/b\\c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i");
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
        // SHA-256 based: first 12 hex chars of hash(asset_id)
        assert_eq!(
            generate_fingerprint_filename("CCPO9c3V/MTwWZJ9bw==", "public.jpeg"),
            "8b2ee97b47e6.JPG"
        );
        assert_eq!(
            generate_fingerprint_filename("ABC", "public.heic"),
            "b5d4045c3f46.HEIC"
        );
        assert_eq!(
            generate_fingerprint_filename("a/b+c=d!e@f#g$h%i", "public.png"),
            "bed2f1035094.PNG"
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
        let mut cache = DirCache::new();
        let found = cache.find_ampm_variant(&query);
        assert!(found.is_some());
        assert_eq!(found.unwrap(), existing);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_ampm_variant_returns_none_for_non_ampm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        let mut cache = DirCache::new();
        assert!(cache.find_ampm_variant(&path).is_none());
    }

    #[test]
    fn test_dir_cache_exists() {
        use std::fs;
        let dir = std::env::temp_dir().join("claude").join("dir_cache_exists");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("photo.jpg"), b"data").unwrap();

        let mut cache = DirCache::new();
        assert!(cache.exists(&dir.join("photo.jpg")));
        assert!(!cache.exists(&dir.join("missing.jpg")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dir_cache_file_size() {
        use std::fs;
        let dir = std::env::temp_dir()
            .join("claude")
            .join("dir_cache_file_size");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("photo.jpg"), b"12345").unwrap();

        let mut cache = DirCache::new();
        assert_eq!(cache.file_size(&dir.join("photo.jpg")), Some(5));
        assert_eq!(cache.file_size(&dir.join("missing.jpg")), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dir_cache_nonexistent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("no_such_subdir/file.jpg");
        let mut cache = DirCache::new();
        assert!(!cache.exists(&nonexistent));
        assert_eq!(cache.file_size(&nonexistent), None);
    }

    #[test]
    fn test_sanitize_path_component_traversal() {
        // clean_filename replaces "/" with "_" so "../etc/passwd" → ".._etc_passwd" → "__etc_passwd"
        assert_eq!(sanitize_path_component("../etc/passwd"), "__etc_passwd");
        assert_eq!(sanitize_path_component(".."), "_");
        // "foo/../bar" → clean replaces "/" → "foo_.._bar" → replace ".." → "foo___bar"
        assert_eq!(sanitize_path_component("foo/../bar"), "foo___bar");
    }

    #[test]
    fn test_sanitize_path_component_dots_and_spaces() {
        // "...hidden..." → clean → "...hidden..." → replace ".." → "_.hidden_." → trim dots → "_.hidden_"
        assert_eq!(sanitize_path_component("...hidden..."), "_.hidden_");
        assert_eq!(sanitize_path_component("  spaced  "), "spaced");
        assert_eq!(sanitize_path_component(".dotfile"), "dotfile");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_sanitize_path_component_reserved_names() {
        assert_eq!(sanitize_path_component("CON"), "_CON");
        assert_eq!(sanitize_path_component("nul"), "_nul");
        assert_eq!(sanitize_path_component("PRN"), "_PRN");
        assert_eq!(sanitize_path_component("COM1"), "_COM1");
        assert_eq!(sanitize_path_component("LPT3"), "_LPT3");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_sanitize_path_component_reserved_names_non_windows() {
        // On non-Windows, reserved names are not prefixed
        assert_eq!(sanitize_path_component("CON"), "CON");
        assert_eq!(sanitize_path_component("nul"), "nul");
        assert_eq!(sanitize_path_component("PRN"), "PRN");
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
            "01d6235dcbf6.unknown"
        );
    }

    #[test]
    fn test_clean_filename_null_bytes() {
        assert_eq!(clean_filename("photo\0.jpg"), "photo_.jpg");
    }

    #[test]
    fn test_clean_filename_empty_string() {
        assert_eq!(clean_filename(""), "");
    }

    #[test]
    fn test_clean_filename_all_invalid_chars() {
        assert_eq!(clean_filename("/:*?\"<>|\\"), "_________");
    }

    #[test]
    fn test_clean_filename_truncates_long_name_with_extension() {
        let long_stem = "a".repeat(300);
        let filename = format!("{long_stem}.jpg");
        let result = clean_filename(&filename);
        assert!(result.len() <= 255);
        assert!(result.ends_with(".jpg"));
    }

    #[test]
    fn test_clean_filename_truncates_long_name_without_extension() {
        let filename = "a".repeat(300);
        let result = clean_filename(&filename);
        assert_eq!(result.len(), 255);
    }

    #[test]
    fn test_clean_filename_no_truncation_at_limit() {
        let filename = format!("{}.jpg", "a".repeat(251));
        assert_eq!(filename.len(), 255);
        assert_eq!(clean_filename(&filename), filename);
    }

    #[test]
    fn test_clean_filename_truncates_multibyte_on_char_boundary() {
        // Each '日' is 3 bytes; ensure we don't split mid-character
        let stem = "日".repeat(100); // 300 bytes
        let filename = format!("{stem}.jpg");
        let result = clean_filename(&filename);
        assert!(result.len() <= 255);
        assert!(result.ends_with(".jpg"));
        // Stem should be truncated to a whole number of 3-byte chars
        let stem_part = &result[..result.len() - 4];
        assert_eq!(stem_part.len() % 3, 0);
    }

    #[test]
    fn test_clean_filename_truncates_oversized_extension() {
        let filename = format!("a.{}", "x".repeat(300));
        let result = clean_filename(&filename);
        assert_eq!(result.len(), 255);
    }

    #[test]
    fn test_sanitize_path_component_control_characters() {
        let result = sanitize_path_component("album\ttab\nnewline");
        assert_eq!(result, "album_tab_newline");
    }

    #[test]
    fn test_sanitize_path_component_long_input() {
        // Very long album names (>255 bytes) are truncated
        let long_name = "a".repeat(1000);
        let result = sanitize_path_component(&long_name);
        assert_eq!(result.len(), 255);
    }

    #[test]
    fn test_sanitize_path_component_unicode_with_traversal() {
        // Unicode album name with path traversal attempt
        assert_eq!(
            sanitize_path_component("日本語/../secrets"),
            "日本語___secrets"
        );
    }

    #[test]
    fn test_local_download_path_none_folder_structure() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_path(dir, "none", &date, "IMG_0001.JPG", None);
        assert_eq!(result, PathBuf::from("/photos/IMG_0001.JPG"));
    }

    #[test]
    fn test_local_download_path_none_case_insensitive() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        assert_eq!(
            local_download_path(dir, "NONE", &date, "photo.jpg", None),
            PathBuf::from("/photos/photo.jpg")
        );
        assert_eq!(
            local_download_path(dir, "None", &date, "photo.jpg", None),
            PathBuf::from("/photos/photo.jpg")
        );
    }

    #[test]
    fn test_local_download_path_date_based_folder() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_path(dir, "{:%Y/%m/%d}", &date, "IMG_0001.JPG", None);
        assert_eq!(result, PathBuf::from("/photos/2025/06/15/IMG_0001.JPG"));
    }

    #[test]
    fn test_local_download_path_without_python_wrapper() {
        // Format string without {: ... } wrapper
        let dir = Path::new("/photos");
        let date = chrono::Local.with_ymd_and_hms(2025, 1, 5, 9, 5, 3).unwrap();
        let result = local_download_path(dir, "%Y-%m-%d", &date, "photo.jpg", None);
        assert_eq!(result, PathBuf::from("/photos/2025-01-05/photo.jpg"));
    }

    #[test]
    fn test_local_download_path_with_time_components() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 12, 31, 23, 59, 59)
            .unwrap();
        let result = local_download_path(dir, "{:%Y/%m/%d/%H-%M-%S}", &date, "photo.jpg", None);
        assert_eq!(
            result,
            PathBuf::from("/photos/2025/12/31/23-59-59/photo.jpg")
        );
    }

    #[test]
    fn test_generate_fingerprint_filename_empty_id() {
        let result = generate_fingerprint_filename("", "public.jpeg");
        assert_eq!(result, "e3b0c44298fc.JPG");
    }

    #[test]
    fn test_local_download_path_cleans_invalid_chars_in_filename() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_path(dir, "none", &date, "photo:1.jpg", None);
        assert_eq!(result, PathBuf::from("/photos/photo_1.jpg"));
    }

    #[test]
    fn test_local_download_path_traversal_in_folder_structure() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();

        // ".." components are neutralised — path stays inside directory
        assert_eq!(
            local_download_path(dir, "../../etc", &date, "passwd", None),
            PathBuf::from("/photos/_/_/etc/passwd")
        );
        assert_eq!(
            local_download_path(dir, "../../%Y", &date, "photo.jpg", None),
            PathBuf::from("/photos/_/_/2025/photo.jpg")
        );
        assert_eq!(
            local_download_path(dir, "{:../../%Y}", &date, "photo.jpg", None),
            PathBuf::from("/photos/_/_/2025/photo.jpg")
        );
    }

    #[test]
    fn test_normalize_ampm_no_break_space_u00a0() {
        // U+00A0 (NO-BREAK SPACE) before AM should also be stripped
        assert_eq!(
            normalize_ampm("Screenshot at 10.30.00\u{00A0}AM.PNG"),
            "Screenshot at 10.30.00AM.PNG"
        );
    }

    #[test]
    fn test_normalize_ampm_lowercase_pm() {
        // AM/PM matching is case-insensitive in the check
        assert_eq!(
            normalize_ampm("Screenshot at 1.40.01 pm.PNG"),
            "Screenshot at 1.40.01pm.PNG"
        );
    }

    #[test]
    fn test_remove_unicode_chars_combining_characters() {
        // U+0301 (combining acute accent) is non-ASCII and gets stripped,
        // but the base 'e' is ASCII and remains
        assert_eq!(remove_unicode_chars("cafe\u{0301}.jpg"), "cafe.jpg");
    }

    #[test]
    fn test_clean_filename_zero_width_space() {
        // U+200B (zero-width space) is not a control char in Rust,
        // so it passes through clean_filename unchanged
        assert_eq!(
            clean_filename("photo\u{200B}name.jpg"),
            "photo\u{200B}name.jpg"
        );
    }

    #[test]
    fn test_clean_filename_zero_width_joiner() {
        // U+200D (zero-width joiner) is not a control char in Rust,
        // so it passes through clean_filename unchanged
        assert_eq!(clean_filename("pic\u{200D}file.jpg"), "pic\u{200D}file.jpg");
    }

    #[test]
    fn test_clean_filename_rtl_mark() {
        // U+200F (right-to-left mark) is not a control char in Rust,
        // so it passes through clean_filename unchanged
        assert_eq!(
            clean_filename("photo\u{200F}name.jpg"),
            "photo\u{200F}name.jpg"
        );
    }

    #[test]
    fn test_remove_unicode_chars_emoji_only_filename() {
        // All emoji are non-ASCII and get stripped, leaving only ".jpg"
        assert_eq!(remove_unicode_chars("🌅🏔️.jpg"), ".jpg");
    }

    #[test]
    fn test_remove_unicode_chars_emoji_only_no_extension() {
        // All emoji are non-ASCII; nothing remains
        assert_eq!(remove_unicode_chars("🌅🏔️"), "");
    }

    #[test]
    fn test_sanitize_path_component_extension_only() {
        // ".jpg" has a leading dot that gets stripped, leaving "jpg"
        assert_eq!(sanitize_path_component(".jpg"), "jpg");
    }

    #[test]
    fn test_sanitize_path_component_all_spaces() {
        // All spaces are trimmed, leaving empty, which falls back to "_"
        assert_eq!(sanitize_path_component("   "), "_");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_sanitize_path_component_windows_reserved_with_extension() {
        assert_eq!(sanitize_path_component("CON.txt"), "_CON.txt");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_sanitize_path_component_reserved_with_extension_non_windows() {
        // On non-Windows, reserved names are not prefixed
        assert_eq!(sanitize_path_component("CON.txt"), "CON.txt");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_sanitize_path_component_windows_reserved_case_insensitive() {
        assert_eq!(sanitize_path_component("con"), "_con");
        assert_eq!(sanitize_path_component("Con"), "_Con");
        assert_eq!(sanitize_path_component("cOn"), "_cOn");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_sanitize_path_component_reserved_case_insensitive_non_windows() {
        // On non-Windows, reserved names are not prefixed
        assert_eq!(sanitize_path_component("con"), "con");
        assert_eq!(sanitize_path_component("Con"), "Con");
        assert_eq!(sanitize_path_component("cOn"), "cOn");
    }

    #[test]
    fn test_clean_filename_mixed_invalid_chars() {
        // '<', '>', '|' are all invalid filesystem chars and get replaced with '_'
        assert_eq!(clean_filename("photo<>|name.jpg"), "photo___name.jpg");
    }

    #[test]
    fn test_clean_filename_newline() {
        // Newline is a control character and gets replaced with '_'
        assert_eq!(clean_filename("photo\nname.jpg"), "photo_name.jpg");
    }

    #[test]
    fn test_clean_filename_tab() {
        // Tab is a control character and gets replaced with '_'
        assert_eq!(clean_filename("photo\tname.jpg"), "photo_name.jpg");
    }

    #[test]
    fn test_strftime_month_name_token() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 1, 15, 10, 0, 0)
            .unwrap();
        let result = local_download_path(dir, "%Y/%B/%d", &date, "photo.jpg", None);
        assert_eq!(result, PathBuf::from("/photos/2025/January/15/photo.jpg"));
    }

    #[test]
    fn test_album_token_expansion() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_path(
            dir,
            "{album}/%Y/%m/%d",
            &date,
            "photo.jpg",
            Some("Vacation"),
        );
        assert_eq!(
            result,
            PathBuf::from("/photos/Vacation/2025/06/15/photo.jpg")
        );
    }

    #[test]
    fn test_album_token_none_becomes_empty() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_path(dir, "{album}/%Y/%m/%d", &date, "photo.jpg", None);
        // Empty album name means the {album} component is empty, so it's skipped
        assert_eq!(result, PathBuf::from("/photos/2025/06/15/photo.jpg"));
    }

    #[test]
    fn test_album_token_empty_string_skipped() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        // The "All Photos" album has name "" -- should behave like None
        let result = local_download_path(dir, "{album}/%Y/%m/%d", &date, "photo.jpg", Some(""));
        assert_eq!(result, PathBuf::from("/photos/2025/06/15/photo.jpg"));
    }

    #[test]
    fn test_album_token_sanitized() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_path(dir, "{album}/%Y", &date, "photo.jpg", Some("../etc"));
        // Path traversal in album name is neutralized
        assert!(!result.to_str().unwrap().contains("../"));
    }

    #[test]
    fn test_album_token_percent_escaped() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        // Album name containing % should not be interpreted as strftime
        let result =
            local_download_path(dir, "{album}/%Y", &date, "photo.jpg", Some("My %d Album"));
        let result_str = result.to_str().unwrap();
        // %d should be literal, not expanded to "15"
        assert!(
            result_str.contains("%d"),
            "percent in album name should be literal, got: {result_str}"
        );
        assert!(
            !result_str.contains("/15/"),
            "album %d should not expand to day number, got: {result_str}"
        );
    }

    #[test]
    fn test_album_token_trailing_percent_no_panic() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        // Trailing % in album name must not panic
        let result = local_download_path(dir, "{album}/%Y", &date, "photo.jpg", Some("50% Off"));
        assert!(result.to_str().unwrap().contains("50% Off"));
    }

    #[test]
    fn test_no_album_token_ignores_album_name() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        // Without {album} in format, album_name is ignored
        let result = local_download_path(dir, "%Y/%m/%d", &date, "photo.jpg", Some("Vacation"));
        assert_eq!(result, PathBuf::from("/photos/2025/06/15/photo.jpg"));
    }
}
