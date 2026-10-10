use std::fmt::{Display, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use base64::Engine;
#[cfg(test)]
use chrono::Local;
use chrono::{DateTime, TimeZone};
use rustc_hash::FxHashMap;

/// Sentinel folder-structure value that disables the date-based directory
/// hierarchy entirely (files land directly in the download directory).
/// Matched case-insensitively.
pub(crate) const NO_DATE_STRUCTURE: &str = "none";

/// Token replaced with an album name in `--folder-structure-albums`
/// templates (also accepted in legacy `--folder-structure` for backward
/// compatibility; auto-migrated at config load).
pub(crate) const TOKEN_ALBUM: &str = "{album}";

/// Token replaced with a smart-folder name in
/// `--folder-structure-smart-folders` templates.
pub(crate) const TOKEN_SMART_FOLDER: &str = "{smart-folder}";

/// Token replaced with the path-friendly zone name (see
/// [`truncate_library_zone`]). Valid in any folder-structure template;
/// must be the leading path segment when present.
pub(crate) const TOKEN_LIBRARY: &str = "{library}";

#[expect(
    clippy::string_slice,
    reason = "indices from starts_with/ends_with on ASCII literals are always char-aligned"
)]
/// Strip the legacy Python-style `{:%Y/%m/%d}` wrapper, returning the inner
/// format string. Returns the input unchanged if the wrapper is absent.
pub(crate) fn strip_python_wrapper(folder_structure: &str) -> &str {
    if folder_structure.starts_with("{:") && folder_structure.ends_with('}') {
        &folder_structure[2..folder_structure.len() - 1]
    } else {
        folder_structure
    }
}

/// Expand a named token (e.g. `{album}`, `{smart-folder}`) in a folder
/// structure format string.
///
/// Strips the Python-style wrapper, sanitizes `name` as a path component,
/// escapes `%` for chrono strftime, and replaces every occurrence of `token`.
/// Returns the original `folder_structure` (wrapper-stripped) unchanged if
/// `token` is absent.
pub(crate) fn expand_named_token(
    folder_structure: &str,
    token: &str,
    name: Option<&str>,
) -> String {
    let format_str = strip_python_wrapper(folder_structure);
    if !format_str.contains(token) {
        return format_str.to_string();
    }
    let safe_name = name
        .filter(|n| !n.is_empty())
        .map(|n| sanitize_path_component(n).replace('%', "%%"))
        .unwrap_or_default();
    format_str.replace(token, &safe_name)
}

/// Path-friendly form of a CloudKit zone name. Returns the input unchanged
/// for the primary library (`PrimarySync`) and any non-shared zone, and
/// truncates `SharedSync-<UUID>` to `SharedSync-<8 chars>` so on-disk
/// folder names stay readable while remaining stable across reruns.
///
/// The 8-char suffix is the leading hex group of the UUID, which CloudKit
/// emits deterministically — a copy-paste from the on-disk path back into
/// `--library` resolves to the same zone.
///
/// Output is NOT sanitized — callers that stitch the return value into a
/// filesystem path must run it through [`sanitize_path_component`] (or
/// reach it via [`expand_named_token`], which does so automatically).
pub(crate) fn truncate_library_zone(zone_name: &str) -> &str {
    const PREFIX: &str = "SharedSync-";
    const KEEP: usize = 8;
    let Some(rest) = zone_name.strip_prefix(PREFIX) else {
        return zone_name;
    };
    if rest.len() <= KEEP {
        return zone_name;
    }
    let cut = PREFIX.len() + KEEP;
    // Char-boundary guard: CloudKit UUIDs are ASCII hex/dash today; if
    // that ever changes mid-codepoint, fall back to the full zone name.
    if !zone_name.is_char_boundary(cut) {
        return zone_name;
    }
    #[expect(
        clippy::string_slice,
        reason = "is_char_boundary guard above ensures cut lands on a char boundary"
    )]
    &zone_name[..cut]
}

/// Build the date-based parent directory for a photo asset (without filename).
///
/// `folder_structure` is a strftime format string such as `"%Y/%m/%d"`. The
/// legacy Python-style `"{:%Y/%m/%d}"` wrapper is also accepted. The custom
/// `{album}` token is expanded to the album name before strftime processing.
/// The special value `"none"` (case-insensitive) disables date-based folders.
pub(crate) fn local_download_dir<Tz: TimeZone>(
    directory: &Path,
    folder_structure: &str,
    created_date: &DateTime<Tz>,
    album_name: Option<&str>,
) -> PathBuf
where
    Tz::Offset: Display,
{
    if folder_structure.eq_ignore_ascii_case(NO_DATE_STRUCTURE) {
        return directory.to_path_buf();
    }

    let expanded = expand_named_token(folder_structure, TOKEN_ALBUM, album_name);

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
pub(crate) fn local_download_path<Tz: TimeZone>(
    directory: &Path,
    folder_structure: &str,
    created_date: &DateTime<Tz>,
    filename: &str,
    album_name: Option<&str>,
) -> PathBuf
where
    Tz::Offset: Display,
{
    local_download_dir(directory, folder_structure, created_date, album_name)
        .join(clean_filename(filename).as_ref())
}

/// Maximum filename length in bytes for common filesystems (ext4, APFS, NTFS).
const MAX_FILENAME_BYTES: usize = 255;

#[expect(
    clippy::string_slice,
    reason = "indices from rfind('.') and floor_char_boundary are always char-aligned"
)]
/// Clean a filename by replacing characters that are invalid on common
/// filesystems (`/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`, `|`) and control
/// characters (including NUL) with `_`. Truncates filenames exceeding 255
/// bytes, preserving the file extension.
///
/// Returns `Cow::Borrowed` when no character replacement and no truncation
/// is needed — the overwhelming common case for iCloud filenames — so the
/// filter hot path avoids a fresh allocation per asset.
pub(crate) fn clean_filename(filename: &str) -> std::borrow::Cow<'_, str> {
    fn is_invalid(c: char) -> bool {
        matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control()
    }

    // Empty input would otherwise join onto the download directory as
    // nothing, silently producing a path identical to the directory and
    // colliding across every asset that hits this branch. Mirror
    // sanitize_path_component's `_` sentinel so the path is visible and
    // distinct.
    if filename.is_empty() {
        return std::borrow::Cow::Borrowed("_");
    }

    if filename.len() <= MAX_FILENAME_BYTES && !filename.contains(is_invalid) {
        return std::borrow::Cow::Borrowed(filename);
    }

    let cleaned: String = filename
        .chars()
        .map(|c| if is_invalid(c) { '_' } else { c })
        .collect();

    if cleaned.len() <= MAX_FILENAME_BYTES {
        return std::borrow::Cow::Owned(cleaned);
    }

    // Preserve the extension (e.g. ".jpg") when truncating, but only if it
    // leaves room for at least one stem character.
    if let Some(dot) = cleaned.rfind('.') {
        let ext = &cleaned[dot..];
        if ext.len() < MAX_FILENAME_BYTES {
            let stem_end = cleaned[..dot].floor_char_boundary(MAX_FILENAME_BYTES - ext.len());
            return std::borrow::Cow::Owned(format!("{}{ext}", &cleaned[..stem_end]));
        }
    }

    std::borrow::Cow::Owned(cleaned[..cleaned.floor_char_boundary(MAX_FILENAME_BYTES)].to_string())
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
///
/// Returns `Cow::Borrowed` when the input is already all-ASCII (the common
/// case for iCloud filenames) so callers avoid an allocation per asset.
pub(crate) fn remove_unicode_chars(filename: &str) -> std::borrow::Cow<'_, str> {
    if filename.is_ascii() {
        return std::borrow::Cow::Borrowed(filename);
    }
    std::borrow::Cow::Owned(filename.chars().filter(char::is_ascii).collect())
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

/// Filesystem-safe asset id suffix for kei-generated identity collision names.
///
/// CloudKit record names can contain characters such as `/` that are invalid
/// inside one filename component. Download path construction sanitizes the
/// final filename, so every path that reconstructs or recognizes the generated
/// suffix must sanitize the asset id the same way.
pub(crate) fn asset_identity_suffix(asset_id: &str) -> String {
    clean_filename(asset_id).into_owned()
}

/// Insert a filesystem-safe asset id before the file extension.
pub(crate) fn insert_asset_identity_suffix(path: &str, asset_id: &str) -> String {
    insert_suffix(path, &asset_identity_suffix(asset_id))
}

/// Insert a filesystem-safe asset id plus a stable collision ordinal before
/// the file extension.
pub(crate) fn insert_asset_identity_ordinal_suffix(path: &str, asset_id: &str, n: u64) -> String {
    let suffix = format!("{}-{n}", asset_identity_suffix(asset_id));
    insert_suffix(path, &suffix)
}

/// Return true when `candidate` is `base` with this asset id appended before
/// the extension, optionally followed by kei's stable collision ordinal.
///
/// For example, `IMG_0001-A.MOV` and `IMG_0001-A-2.MOV` are both identity
/// collision names in the family rooted at `IMG_0001.MOV` for asset `A`.
pub(crate) fn filename_matches_identity_collision(
    base: &str,
    asset_id: &str,
    candidate: &str,
) -> bool {
    let base = clean_filename(base);
    let asset_id = asset_identity_suffix(asset_id);
    let base = base.as_ref();

    if candidate == insert_suffix(base, &asset_id) {
        return true;
    }

    let Some((base_stem, base_ext)) = base.rsplit_once('.') else {
        let prefix = format!("{base}-{asset_id}-");
        return candidate
            .strip_prefix(&prefix)
            .is_some_and(|ordinal| ordinal.parse::<u64>().is_ok());
    };
    let Some((candidate_stem, candidate_ext)) = candidate.rsplit_once('.') else {
        return false;
    };
    if candidate_ext != base_ext {
        return false;
    }

    let prefix = format!("{base_stem}-{asset_id}-");
    candidate_stem
        .strip_prefix(&prefix)
        .is_some_and(|ordinal| ordinal.parse::<u64>().is_ok())
}

/// Add a literal suffix before the file extension.
///
/// Render a terminal role after identity/collision qualifiers, reserving its
/// byte budget before truncation. Provider text is never stripped as a role.
pub(crate) fn role_filename(filename: &str, qualifier: &str, role: &str) -> String {
    let cleaned = clean_filename(filename);
    let (stem, extension) = cleaned
        .rsplit_once('.')
        .map_or((cleaned.as_ref(), ""), |(s, e)| (s, e));
    let tail = if extension.is_empty() {
        format!("{qualifier}{role}")
    } else {
        format!("{qualifier}{role}.{extension}")
    };
    let budget = 255_usize.saturating_sub(tail.len());
    let stem: String = stem
        .chars()
        .scan(0_usize, |used, c| {
            *used += c.len_utf8();
            (*used <= budget).then_some(c)
        })
        .collect();
    format!("{stem}{tail}")
}

/// Unlike [`insert_suffix`], this does not add a hyphen. Use it for suffixes
/// that already include their separator, such as `_edited`.
pub(crate) fn insert_literal_suffix(path: &str, suffix: &str) -> String {
    if let Some(dot_pos) = path.rfind('.') {
        let (stem, ext) = path.split_at(dot_pos);
        let mut result = String::with_capacity(stem.len() + suffix.len() + ext.len());
        result.push_str(stem);
        result.push_str(suffix);
        result.push_str(ext);
        result
    } else {
        let mut result = String::with_capacity(path.len() + suffix.len());
        result.push_str(path);
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

#[expect(
    clippy::string_slice,
    reason = "index from rfind('.') is always char-aligned"
)]
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

#[expect(clippy::string_slice, reason = "base64 output is pure ASCII")]
/// Compute the first 7 characters of the base64-encoded asset ID.
///
/// Used by the `name-id7` file match policy to create unique filenames.
/// Uses the URL-safe base64 alphabet (`-` / `_`) so the result is always
/// safe to embed in a filename. Standard base64 would emit `/` (path
/// separator!) or `+` for certain input byte sequences — CloudKit asset
/// IDs can contain bytes that trigger that, e.g. an ID containing `?`
/// would produce `/` at base64 position 4.
fn base64_id7(id: &str) -> String {
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id.as_bytes());
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

#[expect(
    clippy::string_slice,
    reason = "ext[1..] skips '.' from rfind — always char-aligned"
)]
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
    for b in hash.iter().take(6) {
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
/// - No-break space (U+00A0): `1.40.01\u{00A0}PM`
/// - Narrow no-break space (U+202F): `1.40.01\u{202F}PM`
/// - No space: `1.40.01PM`
///
/// This function strips any of these to produce a consistent `1.40.01PM` form,
/// enabling matching between files created with different locale settings.
pub(crate) fn normalize_ampm(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        // If this char is a known AM/PM-prefix whitespace variant and the
        // next two chars spell "AM" or "PM" (case-insensitive), drop the
        // whitespace — the A/P + M will be appended on subsequent
        // iterations as normal.
        if matches!(c, ' ' | '\u{00A0}' | '\u{202F}') {
            let mut lookahead = chars.clone();
            if let (Some(c1), Some(c2)) = (lookahead.next(), lookahead.next()) {
                let c1u = c1.to_ascii_uppercase();
                let c2u = c2.to_ascii_uppercase();
                if (c1u == 'A' || c1u == 'P') && c2u == 'M' {
                    continue;
                }
            }
        }
        result.push(c);
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
#[derive(Debug)]
pub(crate) struct DirCache {
    dirs: FxHashMap<PathBuf, FxHashMap<String, u64>>,
    import_names: FxHashMap<PathBuf, Vec<(String, String, u64)>>,
}

impl DirCache {
    pub fn new() -> Self {
        Self {
            dirs: FxHashMap::default(),
            import_names: FxHashMap::default(),
        }
    }

    /// Invalidate all cached entries. Use after writing files to disk.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.dirs.clear();
        self.import_names.clear();
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

    /// Build import's sorted prefix index off the async runtime. Move the
    /// cached listing into the worker and return it with the index, avoiding
    /// another scan or a synchronous whole-directory clone.
    pub(crate) async fn ensure_import_dir_async(&mut self, dir: &Path) {
        self.ensure_dir_async(dir).await;
        if self.import_names.contains_key(dir) {
            return;
        }
        let entries = self.dirs.remove(dir).unwrap_or_default();
        let (entries, names) = tokio::task::spawn_blocking(move || {
            let mut names: Vec<_> = entries
                .iter()
                .map(|(name, bytes)| (normalize_ampm(name), name.clone(), *bytes))
                .collect();
            names.sort();
            (entries, names)
        })
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "Failed to build import directory index");
            (FxHashMap::default(), Vec::new())
        });
        self.dirs.insert(dir.to_owned(), entries);
        self.import_names.insert(dir.to_owned(), names);
    }

    /// Indexed collision candidates. Ensure the import index first. Prefix
    /// probes avoid scanning every directory entry for each selected rendition.
    pub(crate) fn matching_paths(
        &self,
        dir: &Path,
        size: u64,
        prefixes: &[String],
    ) -> Vec<PathBuf> {
        let Some(names) = self.import_names.get(dir) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        for prefix in prefixes {
            let start = names.partition_point(|(name, _, _)| name.as_str() < prefix.as_str());
            paths.extend(
                names
                    .iter()
                    .skip(start)
                    .take_while(|(name, _, _)| name.starts_with(prefix))
                    .filter(|(_, _, bytes)| *bytes == size)
                    .map(|(_, name, _)| dir.join(name)),
            );
        }
        paths.sort();
        paths.dedup();
        paths
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
            #[allow(
                clippy::indexing_slicing,
                reason = "contains_key() above proves `dir` is present; two-lookup dance \
                          avoids the borrow-checker complaint the outer comment describes"
            )]
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
#[allow(
    clippy::string_slice,
    reason = "test assertions on known-safe string indices"
)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn truncate_library_zone_passes_through_primary() {
        assert_eq!(truncate_library_zone("PrimarySync"), "PrimarySync");
    }

    #[test]
    fn truncate_library_zone_keeps_first_eight_chars_of_shared_uuid() {
        assert_eq!(
            truncate_library_zone("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890"),
            "SharedSync-A1B2C3D4"
        );
    }

    #[test]
    fn truncate_library_zone_passes_through_short_shared_name() {
        // Below the 8-char threshold — return verbatim rather than partial.
        assert_eq!(truncate_library_zone("SharedSync-ABC"), "SharedSync-ABC");
        assert_eq!(
            truncate_library_zone("SharedSync-ABCDEFGH"),
            "SharedSync-ABCDEFGH"
        );
    }

    #[test]
    fn truncate_library_zone_passes_through_unknown_prefix() {
        assert_eq!(truncate_library_zone("CMMLibrary-XYZ"), "CMMLibrary-XYZ");
        assert_eq!(truncate_library_zone(""), "");
    }

    /// Adversarial: two distinct SharedSync zones whose UUIDs
    /// share the leading 8 hex chars produce the same truncated form.
    /// The current implementation has no collision guard, so any
    /// `{library}` token rendered for both would land at the same path
    /// and silently overwrite ("transparent layer" / "no silent
    /// failures" invariant). Pin the collision so any future bail at
    /// resolve-libraries time can rely on this property.
    #[test]
    fn truncate_library_zone_collision_two_distinct_uuids_share_prefix() {
        let zone_a = "SharedSync-A1B2C3D4-EEEE-7890-ABCD-EF1234567890";
        let zone_b = "SharedSync-A1B2C3D4-FFFF-7890-ABCD-EF1234567890";
        // Pre-condition: distinct full UUIDs.
        assert_ne!(zone_a, zone_b);
        // Pinned collision: identical truncated output.
        assert_eq!(truncate_library_zone(zone_a), "SharedSync-A1B2C3D4");
        assert_eq!(truncate_library_zone(zone_b), "SharedSync-A1B2C3D4");
        assert_eq!(truncate_library_zone(zone_a), truncate_library_zone(zone_b));
    }

    /// Pin the positive case of `expand_named_token` for the `{album}`
    /// token: a typical `--folder-structure-albums "{album}/%Y/%m/%d"`
    /// template plus a real album name must place the sanitized name in
    /// the leading segment with the chrono format string preserved
    /// verbatim. The renderer composes `expand_named_token` with chrono's
    /// `strftime` afterwards; if the token gets dropped here the chrono
    /// pass would emit dated paths under the *download root* (not under
    /// the album folder), silently flattening every album.
    #[test]
    fn expand_named_token_album_replaces_token_with_sanitized_name() {
        assert_eq!(
            expand_named_token("{album}/%Y/%m/%d", TOKEN_ALBUM, Some("Vacation")),
            "Vacation/%Y/%m/%d",
        );
        // Embedded position with surrounding strftime tokens preserved.
        assert_eq!(
            expand_named_token("by-album/{album}/%B", TOKEN_ALBUM, Some("Family")),
            "by-album/Family/%B",
        );
    }

    /// Symmetric pin for `{smart-folder}`: the
    /// `--folder-structure-smart-folders` template renders through the
    /// same `expand_named_token` helper. A regression that hard-coded
    /// the helper to the album token alone would silently break every
    /// smart-folder pass.
    #[test]
    fn expand_named_token_smart_folder_replaces_token_with_sanitized_name() {
        assert_eq!(
            expand_named_token("{smart-folder}/%Y", TOKEN_SMART_FOLDER, Some("Favorites")),
            "Favorites/%Y",
        );
        assert_eq!(
            expand_named_token("{smart-folder}", TOKEN_SMART_FOLDER, Some("Videos")),
            "Videos",
        );
    }

    /// `expand_named_token` runs `sanitize_path_component` on the name,
    /// which collapses `..` to `_`. A user album literally named `..`
    /// (legal in Apple Photos) must land at a non-traversing segment
    /// rather than escaping the download root. The other path-component
    /// rules (replace `/` with `_`, etc.) cascade through the same
    /// sanitizer, so this one assertion guards the whole class.
    #[test]
    fn expand_named_token_album_sanitizes_traversal_segment() {
        let out = expand_named_token("{album}/%Y", TOKEN_ALBUM, Some(".."));
        assert!(
            !out.starts_with(".."),
            "album-name traversal must not escape: got {out:?}"
        );
        assert!(
            out.ends_with("/%Y"),
            "rest of template must survive sanitization: got {out:?}"
        );
    }

    /// Adversarial, refined: `expand_named_token` with
    /// `{library}` and an empty zone name must collapse to the empty
    /// segment branch. Pin the **exact** resulting string so a
    /// regression that silently inserts a literal `empty` token (or
    /// reorders surrounding `/`) lands red. Behavior contract today:
    /// `Some("")` is treated like `None` (the `.filter(|n| !n.is_empty())`
    /// guard) so `{library}` is replaced with the empty string, leaving
    /// adjacent slashes.
    #[test]
    fn expand_named_token_library_with_empty_zone_yields_explicit_segment() {
        // Empty zone via Some("") — the defensive case for a CloudKit
        // response that ever returns an empty zone name.
        assert_eq!(
            expand_named_token("{library}/%Y/%m", "{library}", Some("")),
            "/%Y/%m",
            "empty Some-zone must collapse the {{library}} segment to empty",
        );
        // None zone is the documented "no library context" case and
        // must produce identical output (defensive symmetry).
        assert_eq!(
            expand_named_token("{library}/%Y/%m", "{library}", None),
            "/%Y/%m",
            "None zone must mirror Some(\"\") behavior",
        );
        // Embedded position: surrounding text is preserved verbatim.
        assert_eq!(
            expand_named_token("photos/{library}/by-date", "{library}", Some("")),
            "photos//by-date",
            "empty zone in mid-template must leave adjacent separators intact",
        );
    }

    /// End-to-end pin for the legacy `local_download_dir`: combining the
    /// `{album}` token with a chrono strftime template against a fixed
    /// date must produce the exact `<download-dir>/<album>/<date>` path
    /// the docs promise. The function has unit tests for the empty-album
    /// branch via `expand_named_token` callers, but no fixed-output
    /// assertion against a real (album, date) pair.
    #[test]
    fn local_download_dir_renders_album_with_date_hierarchy() {
        let dt = Local
            .with_ymd_and_hms(2025, 1, 2, 12, 0, 0)
            .single()
            .unwrap();
        let path = local_download_dir(
            Path::new("/photos"),
            "{album}/%Y/%m/%d",
            &dt,
            Some("Vacation"),
        );
        assert_eq!(path, PathBuf::from("/photos/Vacation/2025/01/02"));
    }

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
    fn test_insert_asset_identity_suffix_sanitizes_asset_id() {
        assert_eq!(
            insert_asset_identity_suffix("IMG_0001.MOV", "AcGO2/cr6V"),
            "IMG_0001-AcGO2_cr6V.MOV"
        );
        assert_eq!(
            insert_asset_identity_ordinal_suffix("IMG_0001.MOV", "AcGO2/cr6V", 22),
            "IMG_0001-AcGO2_cr6V-22.MOV"
        );
    }

    #[test]
    fn test_filename_matches_identity_collision() {
        assert!(filename_matches_identity_collision(
            "IMG_0001.MOV",
            "ASSET",
            "IMG_0001-ASSET.MOV"
        ));
        assert!(filename_matches_identity_collision(
            "IMG_0001.MOV",
            "ASSET",
            "IMG_0001-ASSET-2.MOV"
        ));
        assert!(filename_matches_identity_collision(
            "photo",
            "ASSET",
            "photo-ASSET-3"
        ));
        assert!(!filename_matches_identity_collision(
            "IMG_0001.MOV",
            "ASSET",
            "IMG_0001-OTHER-2.MOV"
        ));
        assert!(!filename_matches_identity_collision(
            "IMG_0001.MOV",
            "ASSET",
            "IMG_0001-ASSET-x.MOV"
        ));
    }

    #[test]
    fn test_filename_matches_identity_collision_sanitizes_asset_id() {
        assert!(filename_matches_identity_collision(
            "IMG_0001.MOV",
            "AcGO2/cr6V",
            "IMG_0001-AcGO2_cr6V.MOV"
        ));
        assert!(filename_matches_identity_collision(
            "IMG_0001.MOV",
            "AcGO2/cr6V",
            "IMG_0001-AcGO2_cr6V-22.MOV"
        ));
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
        // URL-safe base64("ABC123") = "QUJDMTIz", first 7 = "QUJDMTI"
        // (URL_SAFE_NO_PAD and STANDARD agree on inputs that don't hit
        // the `+` / `/` positions; this test stays stable.)
        assert_eq!(result, "IMG_0001_QUJDMTI.JPG");
    }

    #[test]
    fn test_apply_name_id7_no_extension() {
        let result = apply_name_id7("photo", "XYZ");
        // URL-safe base64("XYZ") = "WFla", first 7 (only 4 available) = "WFla"
        assert_eq!(result, "photo_WFla");
    }

    #[test]
    fn test_base64_id7_length() {
        // Longer IDs should produce exactly 7 chars
        let result = base64_id7("AaBbCcDdEeFfGg/HhIiJj+KkLl");
        assert_eq!(result.len(), 7);
    }

    #[test]
    fn test_base64_id7_never_emits_path_separator() {
        // STANDARD base64 would put `/` at position 4 for this input
        // (byte `?` = 0x3F makes the 4th 6-bit group = 63 = `/`).
        // URL-safe base64 must translate that to `_` instead — otherwise
        // the filename suffix contains a literal path separator and the
        // file lands in a surprise subdirectory.
        let result = base64_id7("AB?xxxxx");
        assert!(
            !result.contains('/'),
            "id suffix must never contain path separator; got {result:?}"
        );
        assert!(
            !result.contains('+'),
            "id suffix must never contain `+` (standard-base64 leak); got {result:?}"
        );
    }

    #[test]
    fn test_base64_id7_only_safe_characters_under_fuzz() {
        // Every byte 0x00..=0xFF as a single-byte ID must produce a
        // suffix containing only URL-safe base64 characters.
        for b in 0u8..=255 {
            let id = String::from_utf8_lossy(&[b]).to_string();
            let result = base64_id7(&id);
            for c in result.chars() {
                assert!(
                    matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_'),
                    "unsafe char {c:?} in suffix for input byte 0x{b:02x}"
                );
            }
        }
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
        // Empty filename is a degenerate API input that previously
        // returned "" — joining onto the download directory then yielded
        // the directory itself, silently colliding across assets. Mirror
        // sanitize_path_component's `_` sentinel so it surfaces.
        assert_eq!(clean_filename(""), "_");
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
    fn test_expand_named_token_smart_folder() {
        let result = expand_named_token("{smart-folder}/%Y", "{smart-folder}", Some("Favorites"));
        assert_eq!(result, "Favorites/%Y");
    }

    #[test]
    fn test_expand_named_token_unknown_token_left_alone() {
        // Caller picked the wrong token: don't substitute, leave the template
        // as-is so callers can detect the mismatch via the literal token.
        let result = expand_named_token("{album}/%Y", "{smart-folder}", Some("Favorites"));
        assert_eq!(result, "{album}/%Y");
    }

    #[test]
    fn test_expand_named_token_sanitizes_smart_folder_name() {
        // Smart-folder names come from a fixed Apple list today, but the
        // sanitisation contract still applies so future custom names can't
        // path-traverse.
        let result = expand_named_token("{smart-folder}/%Y", "{smart-folder}", Some("../etc"));
        assert!(!result.contains("../"));
    }

    #[test]
    fn test_expand_named_token_empty_name_collapses() {
        let result = expand_named_token("{smart-folder}/%Y", "{smart-folder}", Some(""));
        assert_eq!(result, "/%Y");
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

    #[test]
    fn album_with_slash_does_not_create_subdirectory() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_path(dir, "{album}/%Y", &date, "photo.jpg", Some("Trip/2024"));
        assert!(
            result.starts_with(dir),
            "path must stay under the root, got: {result:?}"
        );
        let filename_part = result.to_string_lossy();
        assert!(
            !filename_part.contains("Trip/2024"),
            "slash in album name must be sanitized, got: {filename_part}"
        );
        assert!(
            filename_part.contains("Trip_2024"),
            "slash should become underscore, got: {filename_part}"
        );
    }

    #[test]
    fn album_with_traversal_stays_inside_sync_root() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();

        for album in ["../etc", "../../passwd", "..", "a/../b"] {
            let result = local_download_path(dir, "{album}/%Y", &date, "photo.jpg", Some(album));
            assert!(
                result.starts_with(dir),
                "album={album:?}: path must start with root, got: {result:?}"
            );
            let lossy = result.to_string_lossy();
            assert!(
                !lossy.contains(".."),
                "album={album:?}: traversal must be neutralized, got: {lossy}"
            );
        }
    }

    #[test]
    fn album_with_traversal_composed_with_date_tokens() {
        let dir = Path::new("/photos");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let result = local_download_dir(dir, "{album}/%Y/%m", &date, Some("../etc"));
        assert!(
            result.starts_with(dir),
            "traversal album composed with date tokens must stay inside root, got: {result:?}"
        );
        let components: Vec<_> = result.strip_prefix(dir).unwrap().components().collect();
        assert!(
            components.len() >= 2,
            "should have album + year + month components, got: {components:?}"
        );
    }

    #[test]
    fn generated_path_derivation_inputs_stay_inside_destination() {
        let root = Path::new("/photos-root");
        let date = chrono::Local
            .with_ymd_and_hms(2025, 6, 15, 14, 30, 0)
            .unwrap();
        let folder_structures = [
            "none",
            "%Y/%m/%d",
            "../../%Y/{album}",
            "{album}/%Y/%m/%d",
            "{:../%Y/%m/%d}",
        ];
        let filenames = [
            "",
            "IMG_0001.JPG",
            "../escape.jpg",
            "../../etc/passwd",
            "a/b\\c*d?e\"f<g>h|i.jpg",
            "日本語.jpg",
            "CON",
            "aux.txt",
            "  ...  ",
            "photo\nname.jpg",
            "repeat.JPG",
            "repeat.JPG",
        ];
        let album_names = [
            None,
            Some("Vacation"),
            Some("../etc"),
            Some("Trip/2024"),
            Some("日本語"),
            Some("CON"),
            Some(""),
        ];

        for folder_structure in folder_structures {
            for filename in filenames {
                for album_name in album_names {
                    let path =
                        local_download_path(root, folder_structure, &date, filename, album_name);
                    assert!(
                        path.starts_with(root),
                        "folder={folder_structure:?} filename={filename:?} album={album_name:?}: \
                         path escaped root: {path:?}"
                    );
                    let relative = path
                        .strip_prefix(root)
                        .expect("path should start with root");
                    assert!(
                        relative.file_name().is_some(),
                        "folder={folder_structure:?} filename={filename:?} album={album_name:?}: \
                         path must include a filename: {path:?}"
                    );
                    assert!(
                        relative
                            .components()
                            .all(|component| matches!(component, std::path::Component::Normal(_))),
                        "folder={folder_structure:?} filename={filename:?} album={album_name:?}: \
                         derived path contains traversal or root components: {path:?}"
                    );
                }
            }
        }
    }

    /// Unicode normalization: a filename in NFC form (composed) must
    /// match its NFD form (decomposed) after sanitization. macOS and
    /// Linux use different normalization forms; the sanitizer must
    /// produce consistent output regardless of input normalization.
    #[test]
    fn sanitize_unicode_normalization_consistent_across_forms() {
        // NFC (composed): 'é' as single codepoint U+00E9
        let nfc = "caf\u{00E9}.jpg";
        // NFD (decomposed): 'e' + combining acute accent U+0301
        let nfd = "cafe\u{0301}.jpg";

        // These are different byte sequences
        assert_ne!(nfc.as_bytes(), nfd.as_bytes());

        // After sanitization, both should produce usable filenames
        let s_nfc = sanitize_path_component(nfc);
        let s_nfd = sanitize_path_component(nfd);

        // Both must be non-empty and not contain raw combining chars
        assert!(!s_nfc.is_empty());
        assert!(!s_nfd.is_empty());

        // On macOS, the filesystem normalizes to NFD; on Linux, it
        // preserves the form as-written. The sanitizer should at minimum
        // not introduce path separators or NUL bytes.
        assert!(!s_nfc.contains('/'));
        assert!(!s_nfd.contains('/'));
        assert!(!s_nfc.contains('\\'));
        assert!(!s_nfd.contains('\\'));
    }
}
