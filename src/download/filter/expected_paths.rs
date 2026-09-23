//! Bare expected paths shared by sync and import, before collision resolution.

use std::borrow::Cow;
use std::path::PathBuf;

use chrono::{DateTime, FixedOffset};

use crate::download::paths;
use crate::icloud::photos::types::AssetVersion;
use crate::state::VersionSizeKey;
use crate::types::{
    AssetItemType, AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
};

use super::VersionsView;
use super::config::PathDerivationSource;
use super::versions::{
    apply_raw_policy, select_alternative_extra, select_edited_extra, select_live_edited_extra,
    select_mov_companion, select_primary,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::download) struct MalformedTaskResource {
    pub(in crate::download) field: Box<str>,
    pub(in crate::download) reason: Box<str>,
}

/// One file sync would write for an asset, with the metadata `import-existing`
/// needs to match it against the local filesystem.
#[derive(Debug, Clone)]
pub(crate) struct ExpectedAssetPath {
    /// Absolute path the file would land at, before any collision/dedup suffix.
    pub(crate) path: PathBuf,
    /// Byte size iCloud reports for this version. Used as the strict-match key.
    pub(crate) size: u64,
    /// iCloud-side checksum (CloudKit format, not SHA256).
    pub(crate) checksum: Box<str>,
    /// Signed CDN URL for the selected cloud version.
    pub(crate) url: Box<str>,
    /// Which version this is (Original, LiveOriginal, Medium, ...). Drives the
    /// state-DB row key and `MediaType` classification.
    pub(crate) version_size: VersionSizeKey,
}

/// Bare expected path for one version, before any on-disk / claimed-path
/// collision resolution. The single source of truth shared by
/// `expected_paths_for` (import) and `filter_asset_to_tasks` (sync).
#[derive(Debug, Clone)]
pub(in crate::download) struct DerivedPath {
    /// Absolute path the file would land at, before any dedup suffix.
    pub(in crate::download) path: PathBuf,
    /// Basename of `path`. Sync's collision layer uses this as the input
    /// to `add_dedup_suffix` / `insert_suffix` when colliding with an
    /// existing different-size file.
    pub(in crate::download) filename: String,
    /// CDN URL for the version. Carried so sync can build a `DownloadTask`
    /// without re-walking `asset.versions()`. Unused by import.
    pub(in crate::download) url: Box<str>,
    pub(in crate::download) checksum: Box<str>,
    pub(in crate::download) size: u64,
    pub(in crate::download) version_size: VersionSizeKey,
    /// True for the primary photo (where AM/PM whitespace variants matter
    /// when matching on disk), false for the MOV companion. Sync's
    /// collision layer threads this into `resolve_download_path`.
    pub(in crate::download) check_ampm_on_disk: bool,
}

/// Real filename if the asset has one, otherwise a deterministic
/// fingerprint synthesized from the asset id + first-version UTI. Borrowed
/// when real, owned when synthesized.
pub(super) fn raw_filename(asset: &crate::icloud::photos::PhotoAsset) -> Cow<'_, str> {
    if let Some(f) = asset.filename() {
        Cow::Borrowed(f)
    } else {
        Cow::Owned(paths::generate_fingerprint_filename(
            asset.id(),
            first_version_asset_type(asset),
        ))
    }
}

fn first_version_asset_type(asset: &crate::icloud::photos::PhotoAsset) -> &str {
    asset
        .versions()
        .first()
        .map_or("", |(_, v)| v.asset_type.as_ref())
}

fn filename_stem_is_empty(filename: &str) -> bool {
    let cleaned = paths::clean_filename(filename);
    let stem = cleaned
        .rsplit_once('.')
        .map_or(cleaned.as_ref(), |(stem, _)| stem);
    stem.is_empty()
}

fn replace_empty_stem_with_fingerprint(
    asset_id: &str,
    asset_type: &str,
    filename: String,
) -> String {
    if !filename_stem_is_empty(&filename) {
        return filename;
    }

    let fallback = paths::generate_fingerprint_filename(asset_id, asset_type);
    let Some((fallback_stem, _)) = fallback.rsplit_once('.') else {
        return fallback;
    };
    let Some((_, ext)) = filename.rsplit_once('.').filter(|(_, ext)| !ext.is_empty()) else {
        return fallback;
    };
    format!("{fallback_stem}.{ext}")
}

fn mapped_version_filename(asset_id: &str, base_filename: &str, asset_type: &str) -> String {
    let mapped = paths::map_filename_extension(base_filename, asset_type);
    replace_empty_stem_with_fingerprint(asset_id, asset_type, mapped)
}

fn usable_asset_base_filename(
    asset: &crate::icloud::photos::PhotoAsset,
    ctx: &DerivationContext<'_>,
) -> String {
    replace_empty_stem_with_fingerprint(
        asset.id(),
        first_version_asset_type(asset),
        ctx.base_filename.clone(),
    )
}

/// Per-asset inputs that don't change between primary and MOV companion
/// derivation.
pub(in crate::download) struct DerivationContext<'a> {
    pub(in crate::download) base_filename: String,
    pub(in crate::download) created_local: DateTime<FixedOffset>,
    pub(in crate::download) versions: VersionsView<'a>,
}

impl<'a> DerivationContext<'a> {
    pub(in crate::download) fn build(
        asset: &'a crate::icloud::photos::PhotoAsset,
        config: &(impl PathDerivationSource + ?Sized),
    ) -> Self {
        let raw = raw_filename(asset);
        let base_filename: String = if config.keep_unicode_in_filenames() {
            raw.into_owned()
        } else {
            paths::remove_unicode_chars(&raw).into_owned()
        };
        Self {
            base_filename,
            created_local: asset.created_local(),
            versions: apply_raw_policy(asset.versions(), config.raw_policy()),
        }
    }
}

fn malformed_for_keys(
    asset: &crate::icloud::photos::PhotoAsset,
    keys: impl IntoIterator<Item = AssetVersionSize>,
) -> Option<MalformedTaskResource> {
    let malformed = asset.malformed_resources();
    keys.into_iter().find_map(|key| {
        malformed
            .iter()
            .find(|resource| resource.version_size == key)
            .map(|resource| MalformedTaskResource {
                field: resource.field.clone(),
                reason: resource.reason.clone(),
            })
    })
}

fn primary_candidate_keys(config: &(impl PathDerivationSource + ?Sized)) -> Vec<AssetVersionSize> {
    let mut keys = Vec::with_capacity(2);
    let Some(requested) = config.resolution().to_asset_version_size() else {
        return keys;
    };
    keys.push(requested);
    if !config.force_resolution() && requested != AssetVersionSize::Original {
        keys.push(AssetVersionSize::Original);
    }
    keys
}

fn live_candidate_keys(config: &(impl PathDerivationSource + ?Sized)) -> Vec<AssetVersionSize> {
    let mut keys = Vec::with_capacity(2);
    keys.push(config.live_resolution());
    if !config.force_resolution() && config.live_resolution() != AssetVersionSize::LiveOriginal {
        keys.push(AssetVersionSize::LiveOriginal);
    }
    keys
}

/// Return the malformed resource that explains a post-filter asset producing
/// no downloadable tasks, if CloudKit advertised a selected resource but made
/// it unusable.
pub(in crate::download) fn malformed_no_task_resource(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Option<MalformedTaskResource> {
    if !derive_expected_paths(asset, config).is_empty() {
        return None;
    }

    if (!matches!(
        config.live_photo_mode(),
        LivePhotoMode::Skip | LivePhotoMode::VideoOnly
    ) || !asset.is_live_photo())
        && let Some(resource) = malformed_for_keys(asset, primary_candidate_keys(config))
    {
        return Some(resource);
    }

    if matches!(
        config.live_photo_mode(),
        LivePhotoMode::Both | LivePhotoMode::VideoOnly
    ) && asset.item_type() == Some(AssetItemType::Image)
        && let Some(resource) = malformed_for_keys(asset, live_candidate_keys(config))
    {
        return Some(resource);
    }

    None
}

/// Build the primary `DerivedPath` (or `None` if no primary should be
/// emitted under this config — Skip-mode live photo, VideoOnly mode,
/// or no usable version under `force_resolution`).
pub(in crate::download) fn derive_primary(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
) -> Option<DerivedPath> {
    let (version, effective_size) = select_primary(asset, config, &ctx.versions)?;

    let mapped = mapped_version_filename(asset.state_id(), &ctx.base_filename, &version.asset_type);
    let sized = match effective_size {
        AssetVersionSize::Medium => paths::insert_suffix(&mapped, "medium"),
        AssetVersionSize::Thumb => paths::insert_suffix(&mapped, "thumb"),
        _ => mapped,
    };
    let filename = match config.file_match_policy() {
        FileMatchPolicy::NameId7 => paths::apply_name_id7(&sized, asset.state_id()),
        FileMatchPolicy::NameSizeDedupWithSuffix => sized,
    };
    let path = paths::local_download_path(
        config.directory(),
        config.folder_structure(),
        &ctx.created_local,
        &filename,
        config.album_name(),
    );

    Some(DerivedPath {
        path,
        filename,
        url: version.url.clone(),
        checksum: version.checksum.clone(),
        size: version.size,
        version_size: VersionSizeKey::from(effective_size),
        check_ampm_on_disk: true,
    })
}

fn boxed_url_seen(version: &AssetVersion, seen_urls: &[Box<str>]) -> bool {
    seen_urls
        .iter()
        .any(|seen| seen.as_ref() == version.url.as_ref())
}

fn derive_suffixed_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    version: &AssetVersion,
    key: AssetVersionSize,
    suffix: &str,
    check_ampm_on_disk: bool,
) -> DerivedPath {
    let mapped = mapped_version_filename(asset.state_id(), &ctx.base_filename, &version.asset_type);
    let suffixed = paths::insert_literal_suffix(&mapped, suffix);
    let filename = match config.file_match_policy() {
        FileMatchPolicy::NameId7 => paths::apply_name_id7(&suffixed, asset.state_id()),
        FileMatchPolicy::NameSizeDedupWithSuffix => suffixed,
    };
    let path = paths::local_download_path(
        config.directory(),
        config.folder_structure(),
        &ctx.created_local,
        &filename,
        config.album_name(),
    );
    DerivedPath {
        path,
        filename,
        url: version.url.clone(),
        checksum: version.checksum.clone(),
        size: version.size,
        version_size: VersionSizeKey::from(key),
        check_ampm_on_disk,
    }
}

pub(in crate::download) fn derive_edited_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    seen_urls: &[Box<str>],
) -> Option<DerivedPath> {
    let (version, key) = select_edited_extra(config, &ctx.versions, &[])?;
    if boxed_url_seen(version, seen_urls) {
        return None;
    }
    Some(derive_suffixed_extra(
        asset, config, ctx, version, key, "_edited", true,
    ))
}

pub(in crate::download) fn derive_alternative_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    seen_urls: &[Box<str>],
) -> Option<DerivedPath> {
    let (version, key) = select_alternative_extra(asset, config, &ctx.versions, &[])?;
    if boxed_url_seen(version, seen_urls) {
        return None;
    }
    let suffix = if version.asset_type.contains("raw") {
        "_RAW"
    } else {
        "_alt"
    };
    Some(derive_suffixed_extra(
        asset, config, ctx, version, key, suffix, true,
    ))
}

pub(in crate::download) fn derive_live_edited_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    seen_urls: &[Box<str>],
) -> Option<DerivedPath> {
    let (version, key) = select_live_edited_extra(asset, config, &ctx.versions, &[])?;
    if boxed_url_seen(version, seen_urls) {
        return None;
    }
    Some(derive_suffixed_extra(
        asset, config, ctx, version, key, "_edited", false,
    ))
}

/// Build the live-photo MOV companion `DerivedPath` (or `None` when no
/// MOV applies — non-image asset, Skip / ImageOnly mode, no live version
/// available).
///
/// `primary_effective_filename` is the filename the primary lands at:
/// import passes the *derived* primary filename (no collision yet);
/// sync passes the *resolved* primary filename (after dedup suffix, if
/// any), so a dedup'd primary keeps its MOV paired by filename stem.
pub(in crate::download) fn derive_mov_companion(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    primary_effective_filename: Option<&str>,
) -> Option<DerivedPath> {
    let (live_version, effective_live_size) =
        select_mov_companion(asset, config, &ctx.versions, &[])?;

    let live_base = match config.file_match_policy() {
        FileMatchPolicy::NameId7 => {
            let base = usable_asset_base_filename(asset, ctx);
            paths::apply_name_id7(&base, asset.state_id())
        }
        FileMatchPolicy::NameSizeDedupWithSuffix => primary_effective_filename.map_or_else(
            || usable_asset_base_filename(asset, ctx),
            ToString::to_string,
        ),
    };
    let mov_filename = match config.live_photo_mov_filename_policy() {
        LivePhotoMovFilenamePolicy::Suffix => paths::live_photo_mov_path_suffix(&live_base),
        LivePhotoMovFilenamePolicy::Original => paths::live_photo_mov_path_original(&live_base),
    };
    let mov_path = paths::local_download_path(
        config.directory(),
        config.folder_structure(),
        &ctx.created_local,
        &mov_filename,
        config.album_name(),
    );

    Some(DerivedPath {
        path: mov_path,
        filename: mov_filename,
        url: live_version.url.clone(),
        checksum: live_version.checksum.clone(),
        size: live_version.size,
        version_size: VersionSizeKey::from(effective_live_size),
        check_ampm_on_disk: false,
    })
}

/// Compute the bare expected paths sync would produce for an asset under
/// the given config, without doing collision resolution or disk I/O.
///
/// Returns up to two entries: the primary version and an optional
/// live-photo MOV companion. Empty result means no version applies
/// (`force_resolution` + size unavailable, image-only asset under VideoOnly
/// mode, or live-photo Skip mode).
///
/// Caller must invoke [`crate::download::filter::is_asset_filtered`] first to apply content/date
/// filters; this function only handles version selection + filename
/// derivation.
pub(in crate::download) fn derive_expected_paths(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Vec<DerivedPath> {
    if !asset.has_valid_id() {
        return Vec::new();
    }

    let ctx = DerivationContext::build(asset, config);
    let mut out = Vec::<DerivedPath>::with_capacity(5);
    let mut seen_urls = Vec::<Box<str>>::with_capacity(4);
    let mut primary_index: Option<usize> = None;
    if let Some(p) = derive_primary(asset, config, &ctx) {
        seen_urls.push(p.url.clone());
        out.push(p);
        primary_index = Some(out.len() - 1);
    }
    if let Some(p) = derive_edited_extra(asset, config, &ctx, &seen_urls) {
        seen_urls.push(p.url.clone());
        out.push(p);
    }
    if let Some(p) = derive_alternative_extra(asset, config, &ctx, &seen_urls) {
        seen_urls.push(p.url.clone());
        out.push(p);
    }
    if let Some(p) = derive_live_edited_extra(asset, config, &ctx, &seen_urls) {
        seen_urls.push(p.url.clone());
        out.push(p);
    }
    let primary_filename = primary_index
        .and_then(|index| out.get(index))
        .map(|p| p.filename.as_str());
    if let Some(mov) = derive_mov_companion(asset, config, &ctx, primary_filename) {
        if seen_urls
            .iter()
            .any(|seen| seen.as_ref() == mov.url.as_ref())
        {
            return out;
        }
        out.push(mov);
    }
    out
}

/// Compute the file paths sync would produce for an asset under the given
/// config, mapped to the `ExpectedAssetPath` shape `import-existing`
/// consumes. Thin wrapper over [`derive_expected_paths`].
pub(crate) fn expected_paths_for(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
) -> Vec<ExpectedAssetPath> {
    derive_expected_paths(asset, config)
        .into_iter()
        .map(|d| ExpectedAssetPath {
            path: d.path,
            size: d.size,
            checksum: d.checksum,
            url: d.url,
            version_size: d.version_size,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use crate::download::{DownloadConfig, paths};
    use crate::icloud::photos::PhotoAsset;
    use crate::state::VersionSizeKey;
    use crate::test_helpers::TestPhotoAsset;
    use crate::types::{
        AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy,
    };

    use super::super::config::PathDerivationConfig;
    use super::super::test_support::{filter_asset_fresh, test_config};
    use super::{ExpectedAssetPath, expected_paths_for};

    /// Live-photo HEIC primary with both LiveOriginal and LiveMedium MOV
    /// companions. Covers the live_resolution=Medium path.
    fn live_photo_multi_size_asset(record: &str) -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": record, "fields": {
                "filenameEnc": {"value": "IMG_LIVE.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 4000_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_orig",
                    "fileChecksum": "live_orig_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resVidMedRes": {"value": {
                    "size": 1500_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_med",
                    "fileChecksum": "live_med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"},
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        )
    }

    // ── size / live_resolution matrix on present versions ───────────────
    //
    // The matrix expansion below pins behaviour for [photos].resolution and
    // [photos].live_resolution when the requested version is published. The
    // pre-existing `expected_paths_size_*` tests cover the fallback (resolution
    // missing) and force_resolution branches; these cover the "actually use the
    // requested resolution" branch and the independence between primary and
    // live-photo resolution.

    /// Builds a primary photo with original + medium + thumb JPEG
    /// resolutions. Mirrors `multi_size_photo_asset` (defined later in
    /// this mod) but is independent so test reordering can't break it.
    fn primary_multi_size_asset(record: &str, filename: &str) -> PhotoAsset {
        PhotoAsset::new(
            json!({"recordName": record, "fields": {
                "filenameEnc": {"value": filename, "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000_u64,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "orig_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGMedRes": {"value": {
                    "size": 2000_u64,
                    "downloadURL": "https://p01.icloud-content.com/med",
                    "fileChecksum": "med_ck"
                }},
                "resJPEGMedFileType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 500_u64,
                    "downloadURL": "https://p01.icloud-content.com/thumb",
                    "fileChecksum": "thumb_ck"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        )
    }

    // ── expected_paths_for / filter_asset_to_tasks parity ────────────────
    //
    // expected_paths_for is import-existing's view of where sync would have
    // written each asset. filter_asset_to_tasks is sync's source of truth
    // for the same derivation. They must agree on the bare path (before
    // collision-suffix resolution) so import-existing scans the file sync
    // actually produces. These tests pin parity across the configurations
    // most likely to drift apart (file_match_policy, resolution variants, live
    // photo modes, raw alignment).

    fn assert_path_parity(
        asset: &PhotoAsset,
        config: &DownloadConfig,
        which: VersionSizeKey,
        label: &str,
    ) {
        let want_live = matches!(which, VersionSizeKey::LiveOriginal);
        let expected = expected_paths_for(asset, config);
        let tasks = filter_asset_fresh(asset, config);
        let exp = expected
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveOriginal) == want_live)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let got = tasks
            .iter()
            .find(|t| matches!(t.version_size, VersionSizeKey::LiveOriginal) == want_live)
            .map(|t| t.download_path.to_path_buf())
            .unwrap_or_default();
        assert_eq!(
            exp, got,
            "{label}: expected_paths_for path drifted from filter_asset_to_tasks"
        );
    }

    // ── expected_paths_for tests ────────────────────────────────────────
    //
    // These cover `import-existing`'s view of sync's filename derivation:
    // file_match_policy, size suffix, live photo MOV companion, raw alignment,
    // force_resolution, keep_unicode. Sync's `filter_asset_to_tasks` is the source
    // of truth; collision/dedup-suffix handling is intentionally NOT replayed
    // here (callers don't have claimed_paths state to consult).

    #[test]
    fn expected_paths_default_returns_one_original_path() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].size, 1000);
        assert_eq!(&*paths[0].checksum, "abc123");
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        assert!(
            paths[0].path.to_string_lossy().ends_with("IMG_0001.JPG"),
            "expected ...IMG_0001.JPG, got {}",
            paths[0].path.display()
        );
    }

    #[test]
    fn expected_paths_apply_name_id7_suffix_to_primary() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("IMG_0001_") && name.ends_with(".JPG"),
            "expected IMG_0001_<id7>.JPG, got {name}"
        );
        assert_ne!(name, "IMG_0001.JPG", "id7 suffix not applied");
    }

    #[test]
    fn expected_paths_live_photo_yields_primary_and_mov() {
        let asset = TestPhotoAsset::new("LIVE_1")
            .filename("IMG_2000.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        assert_eq!(paths[1].version_size, VersionSizeKey::LiveOriginal);
        assert_eq!(paths[1].size, 3000);
        assert!(
            paths[1]
                .path
                .to_string_lossy()
                .ends_with("IMG_2000_HEVC.MOV"),
            "expected ...IMG_2000_HEVC.MOV, got {}",
            paths[1].path.display()
        );
    }

    #[test]
    fn expected_paths_video_only_skips_primary() {
        let asset = TestPhotoAsset::new("LIVE_2")
            .filename("IMG_2001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::LiveOriginal);
    }

    #[test]
    fn expected_paths_image_only_skips_mov() {
        let asset = TestPhotoAsset::new("LIVE_3")
            .filename("IMG_2002.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
    }

    /// `LivePhotoMode::Skip` is documented as "skip live photos entirely (both
    /// image and MOV)." A live-photo asset under Skip must yield no paths so
    /// import-existing doesn't scan for files sync never wrote.
    #[test]
    fn expected_paths_skip_mode_emits_nothing_for_live_photo() {
        let asset = TestPhotoAsset::new("LIVE_4")
            .filename("IMG_2003.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::Skip;
        let paths = expected_paths_for(&asset, &config);
        assert!(
            paths.is_empty(),
            "Skip + live photo must drop the asset, got {paths:?}"
        );
    }

    /// Skip applies only to live photos: a non-live asset under Skip still
    /// produces its primary path.
    #[test]
    fn expected_paths_skip_mode_keeps_non_live_primary() {
        let asset = TestPhotoAsset::new("STILL_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::Skip;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
    }

    #[test]
    fn expected_paths_force_resolution_missing_returns_empty() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = true;
        let paths = expected_paths_for(&asset, &config);
        assert!(
            paths.is_empty(),
            "force_resolution + missing size should yield no paths, got {paths:?}"
        );
    }

    #[test]
    fn path_derivation_config_matches_download_config_expected_paths() {
        let asset = TestPhotoAsset::new("PARITY_1")
            .filename("über.heic")
            .live_photo("https://p01.icloud-content.com/live", "live123", 700)
            .adjusted_version(
                "https://p01.icloud-content.com/adjusted",
                "adjusted123",
                900,
                "public.heic",
            )
            .live_adjusted(
                "https://p01.icloud-content.com/live-adjusted",
                "live-adjusted123",
                650,
            )
            .build();
        let mut sync_config = test_config();
        sync_config.folder_structure = "{album}/%Y/%m".to_string();
        sync_config.album_name = Some(Arc::from("Trips"));
        sync_config.edited = true;
        sync_config.file_match_policy = FileMatchPolicy::NameId7;
        sync_config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        sync_config.keep_unicode_in_filenames = true;

        let fields = crate::config::PathDerivationFields {
            folder_structure: sync_config.folder_structure.clone(),
            folder_structure_albums: sync_config.folder_structure_albums.to_string(),
            folder_structure_smart_folders: sync_config.folder_structure_smart_folders.to_string(),
            resolution: sync_config.resolution,
            live_photo_mode: sync_config.live_photo_mode,
            live_resolution: crate::types::LivePhotoResolution::Original,
            live_photo_mov_filename_policy: sync_config.live_photo_mov_filename_policy,
            edited: sync_config.edited,
            alternative: sync_config.alternative,
            raw_policy: sync_config.raw_policy,
            file_match_policy: sync_config.file_match_policy,
            force_resolution: sync_config.force_resolution,
            keep_unicode_in_filenames: sync_config.keep_unicode_in_filenames,
        };
        let mut path_config = PathDerivationConfig::from_path_fields(
            Arc::clone(&sync_config.directory),
            fields,
            sync_config.media,
        );
        path_config.album_name = sync_config.album_name.clone();

        let to_tuples = |paths: Vec<ExpectedAssetPath>| {
            paths
                .into_iter()
                .map(|p| (p.path, p.size, p.checksum, p.url, p.version_size))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            to_tuples(expected_paths_for(&asset, &path_config)),
            to_tuples(expected_paths_for(&asset, &sync_config)),
            "import path config must derive the same expected paths as sync config"
        );
    }

    #[test]
    fn expected_paths_size_fallback_to_original_when_force_resolution_off() {
        let asset = TestPhotoAsset::new("TEST_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains("-medium"),
            "fallback to Original should not carry medium suffix, got {name}"
        );
    }

    #[test]
    fn expected_paths_live_photo_with_name_id7_applies_suffix_to_both() {
        let asset = TestPhotoAsset::new("LIVE_5")
            .filename("IMG_3000.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        let primary = paths[0].path.file_name().unwrap().to_string_lossy();
        let mov = paths[1].path.file_name().unwrap().to_string_lossy();
        assert!(
            primary.starts_with("IMG_3000_") && primary.ends_with(".HEIC"),
            "primary missing id7 suffix: {primary}"
        );
        assert!(
            mov.starts_with("IMG_3000_") && mov.ends_with("_HEVC.MOV"),
            "MOV companion missing id7 suffix: {mov}"
        );
    }

    #[test]
    fn expected_paths_no_versions_returns_empty() {
        // Build a minimal asset with no resOriginalRes — all version lookups
        // fail, expected_paths_for returns empty (caller skips).
        let master = json!({
            "recordName": "EMPTY_1",
            "fields": {
                "filenameEnc": {"value": "x.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
            },
        });
        let asset_record = json!({
            "fields": {"assetDate": {"value": 1736899200000.0_f64}},
        });
        let asset = PhotoAsset::new(master, asset_record);
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert!(paths.is_empty());
    }

    #[test]
    fn expected_paths_parity_default_config() {
        let asset = TestPhotoAsset::new("PAR_1")
            .filename("IMG_5001.JPG")
            .build();
        let config = test_config();
        assert_path_parity(&asset, &config, VersionSizeKey::Original, "default");
    }

    #[test]
    fn expected_paths_parity_unicode_stripped_fingerprint_fallback() {
        let asset = TestPhotoAsset::new("PAR_UNI")
            .filename("日本語.jpg")
            .build();
        let config = test_config();
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "unicode stripped fingerprint fallback",
        );
    }

    #[test]
    fn expected_paths_parity_name_id7() {
        let asset = TestPhotoAsset::new("PAR_2")
            .filename("IMG_5002.JPG")
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        assert_path_parity(&asset, &config, VersionSizeKey::Original, "NameId7");
    }

    #[test]
    fn expected_paths_parity_size_medium_with_fallback() {
        // size=Medium but no medium version available; both call sites
        // must fall back to Original consistently (force_resolution=false).
        let asset = TestPhotoAsset::new("PAR_3")
            .filename("IMG_5003.JPG")
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
        assert_path_parity(&asset, &config, VersionSizeKey::Original, "Medium fallback");
    }

    #[test]
    fn expected_paths_parity_live_photo_both() {
        let asset = TestPhotoAsset::new("PAR_4")
            .filename("IMG_5004.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let config = test_config();
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "live both primary",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "live both mov",
        );
    }

    #[test]
    fn expected_paths_parity_live_photo_name_id7() {
        let asset = TestPhotoAsset::new("PAR_5")
            .filename("IMG_5005.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "live id7 primary",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "live id7 mov",
        );
    }

    #[test]
    fn expected_paths_parity_live_photo_video_only() {
        // VideoOnly: primary path absent in both, MOV present in both.
        let asset = TestPhotoAsset::new("PAR_6")
            .filename("IMG_5006.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mode = LivePhotoMode::VideoOnly;
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "video-only primary (absent)",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "video-only mov",
        );
    }

    #[test]
    fn expected_paths_parity_mov_filename_policy_original() {
        // The non-default MOV filename policy is a known drift suspect:
        // the live_photo_mov_path_original branch in expected_paths_for
        // reuses a helper from paths.rs that filter_asset_to_tasks also
        // calls; this pins them.
        let asset = TestPhotoAsset::new("PAR_7")
            .filename("IMG_5007.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "mov policy=Original primary",
        );
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::LiveOriginal,
            "mov policy=Original mov",
        );
    }

    #[test]
    fn expected_paths_parity_custom_album_in_folder_template() {
        let asset = TestPhotoAsset::new("PAR_8")
            .filename("IMG_5008.JPG")
            .build();
        let mut config = test_config();
        config.folder_structure = "{album}/%Y".to_string();
        config.album_name = Some(Arc::from("Vacation 2025"));
        assert_path_parity(
            &asset,
            &config,
            VersionSizeKey::Original,
            "album in template",
        );
    }

    /// CG-2: regression-guards `resolution = "medium"` actually-published path.
    /// A bug in the size-suffix branch of `expected_paths_for` would emit
    /// an unsuffixed path; sync would write `IMG-medium.JPG` while
    /// import-existing scans for `IMG.JPG` (silent miss).
    #[test]
    fn expected_paths_size_medium_present_emits_medium_suffix() {
        let asset = primary_multi_size_asset("MED_PRESENT", "IMG_6001.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Medium);
        assert_eq!(paths[0].size, 2000);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains("-medium"),
            "size=Medium present should carry '-medium' suffix, got {name}"
        );
    }

    /// CG-2 parity: when Medium is published and `resolution = "medium"` is set,
    /// sync's path and import's path agree.
    #[test]
    fn expected_paths_parity_size_medium_present() {
        let asset = primary_multi_size_asset("PAR_MED", "IMG_6002.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        assert_path_parity(&asset, &config, VersionSizeKey::Medium, "Medium present");
    }

    /// CG-3: regression-guards `resolution = "thumb"` actually-published path.
    #[test]
    fn expected_paths_size_thumb_present_emits_thumb_suffix() {
        let asset = primary_multi_size_asset("THUMB_PRESENT", "IMG_6003.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Thumb;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].version_size, VersionSizeKey::Thumb);
        assert_eq!(paths[0].size, 500);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains("-thumb"),
            "size=Thumb present should carry '-thumb' suffix, got {name}"
        );
    }

    /// CG-3 parity.
    #[test]
    fn expected_paths_parity_size_thumb_present() {
        let asset = primary_multi_size_asset("PAR_THUMB", "IMG_6004.JPG");
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Thumb;
        assert_path_parity(&asset, &config, VersionSizeKey::Thumb, "Thumb present");
    }

    /// CG-4: regression-guards `live_resolution = "medium"`. A bug in the
    /// `version_with_fallback` call inside the live branch would silently
    /// land the LiveOriginal MOV at the LiveMedium config, producing the
    /// wrong path.
    #[test]
    fn expected_paths_live_resolution_medium_emits_live_medium_path() {
        let asset = live_photo_multi_size_asset("LIVE_MED_1");
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveMedium;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2, "expected primary + MOV companion");
        let mov = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveMedium))
            .expect("LiveMedium MOV path missing");
        assert_eq!(mov.size, 1500);
        assert_eq!(&*mov.checksum, "live_med_ck");
        // The primary stays at Original and unaffected by live_resolution.
        let primary = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::Original))
            .expect("primary path missing");
        assert_eq!(primary.version_size, VersionSizeKey::Original);
        assert_eq!(primary.size, 4000);
    }

    /// CG-5: `resolution` and `live_resolution` are independent. A
    /// regression that couples them (e.g. live branch reading
    /// `config.resolution` instead of `config.live_resolution`) lands silently
    /// without this assertion.
    #[test]
    fn expected_paths_size_medium_with_live_resolution_thumb_independent() {
        // Build a HEIC primary with original + medium res, and a live MOV
        // companion at LiveOriginal + LiveMedium. We don't have a
        // LiveThumb resolution to point to, so we use LiveMedium for the
        // live resolution and Medium for the primary -- different
        // non-default values across the two settings.
        let asset = PhotoAsset::new(
            json!({"recordName": "INDEP_1", "fields": {
                "filenameEnc": {"value": "IMG_INDEP.HEIC", "type": "STRING"},
                "itemType": {"value": "public.heic"},
                "resOriginalRes": {"value": {
                    "size": 4000_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_orig",
                    "fileChecksum": "heic_ck"
                }},
                "resOriginalFileType": {"value": "public.heic"},
                "resJPEGMedRes": {"value": {
                    "size": 1800_u64,
                    "downloadURL": "https://p01.icloud-content.com/heic_med",
                    "fileChecksum": "heic_med_ck"
                }},
                "resJPEGMedFileType": {"value": "public.jpeg"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_orig",
                    "fileChecksum": "live_orig_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resVidMedRes": {"value": {
                    "size": 1500_u64,
                    "downloadURL": "https://p01.icloud-content.com/live_med",
                    "fileChecksum": "live_med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"},
            }}),
            json!({"fields": {"assetDate": {"value": 1_736_899_200_000.0_f64}}}),
        );
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.live_resolution = AssetVersionSize::LiveMedium;

        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        let primary = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::Medium))
            .expect("primary at Medium missing");
        assert_eq!(primary.size, 1800);
        let mov = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveMedium))
            .expect("MOV at LiveMedium missing");
        assert_eq!(mov.size, 1500);
        // Crucially: primary did not key off live_resolution, MOV did
        // not key off primary resolution. (If the two settings were coupled, both would
        // share one variant.)
        assert_ne!(primary.version_size, VersionSizeKey::LiveMedium);
        assert_ne!(mov.version_size, VersionSizeKey::Medium);
    }

    /// CG-6: `raw_policy = "prefer-raw"` + `resolution = "medium"`. apply_raw_policy
    /// runs before size selection; this pins that the swap doesn't
    /// silently re-key the size lookup off the wrong version.
    #[test]
    fn expected_paths_raw_policy_prefer_raw_with_resolution_medium_keys_correctly() {
        // RAW + JPEG pair where the alt is the JPEG. With
        // raw_policy=PreferRaw promotes the RAW side into the Original slot
        // (the "user-visible" original) per the existing `raw_policy_*`
        // tests; the question here is whether `resolution = "medium"` then keys
        // off the Medium version of that promoted RAW side (the test's primary
        // has no medium published, so we expect fallback to Original size with
        // force_resolution=false).
        let asset = TestPhotoAsset::new("ALIGN_MED")
            .filename("IMG_RAW_MED.DNG")
            .item_type("public.camera-raw-image")
            .orig_file_type("public.camera-raw-image")
            .alt_version(
                "https://p01.icloud-content.com/jpeg",
                "jpeg_ck",
                2500,
                "public.jpeg",
            )
            .build();
        let mut config = test_config();
        config.raw_policy = RawPolicy::PreferRaw;
        config.resolution = crate::types::PhotoResolution::Medium;
        config.force_resolution = false;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        // No medium published in the swapped Original (which is the
        // RAW alt promoted to Original under PreferRaw); the
        // fallback should land on Original-without-suffix.
        assert_eq!(paths[0].version_size, VersionSizeKey::Original);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains("-medium"),
            "fallback to Original under raw_policy must drop the medium suffix, got {name}"
        );
    }

    /// CG-7: `force_resolution = true` applied to the live-photo companion. With
    /// force_resolution=true and live_resolution=LiveMedium but only LiveOriginal
    /// published, the MOV companion should drop entirely (not silently
    /// land at LiveOriginal).
    #[test]
    fn expected_paths_force_resolution_drops_live_companion_when_live_size_missing() {
        let asset = TestPhotoAsset::new("FORCE_LIVE")
            .filename("IMG_FL.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/live_orig", "live_ck", 3000)
            .build();
        let mut config = test_config();
        config.live_resolution = AssetVersionSize::LiveMedium;
        config.force_resolution = true;
        let paths = expected_paths_for(&asset, &config);
        // Primary HEIC is still Original and kept (force_resolution applies to
        // the requested primary resolution and to the requested
        // `live_resolution`; primary resolution is Original which is
        // present).
        assert!(
            paths
                .iter()
                .any(|p| matches!(p.version_size, VersionSizeKey::Original)),
            "primary should remain present when its requested size is published"
        );
        // The MOV companion should drop because LiveMedium is missing
        // and force_resolution=true forbids fallback.
        assert!(
            !paths.iter().any(|p| matches!(
                p.version_size,
                VersionSizeKey::LiveOriginal | VersionSizeKey::LiveMedium
            )),
            "force_resolution + missing LiveMedium should drop the MOV companion entirely, got {paths:?}"
        );
    }

    /// CG-8: `--live-photo-mov-filename-policy original` + `name-id7`.
    /// The Original-policy branch must still apply the name-id7 suffix
    /// to the MOV (otherwise id7 users on the non-default MOV policy
    /// silently break).
    #[test]
    fn expected_paths_mov_policy_original_with_name_id7_carries_suffix() {
        let asset = TestPhotoAsset::new("MOV_ID7_ORIG")
            .filename("IMG_8001.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .live_photo("https://p01.icloud-content.com/mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.file_match_policy = FileMatchPolicy::NameId7;
        config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 2);
        let mov = paths
            .iter()
            .find(|p| matches!(p.version_size, VersionSizeKey::LiveOriginal))
            .expect("MOV companion missing");
        let mov_name = mov.path.file_name().unwrap().to_string_lossy();
        // Under Original policy the MOV reuses the primary's stem (no
        // _HEVC suffix); under NameId7 the stem itself carries the id7
        // marker, so the MOV path must carry it too. The HEIC->MOV
        // extension map happens regardless of policy.
        assert!(
            mov_name.starts_with("IMG_8001_") && mov_name.ends_with(".MOV"),
            "MOV under Original policy + id7 should be IMG_8001_<id7>.MOV, got {mov_name}"
        );
        assert!(
            !mov_name.contains("_HEVC"),
            "Original MOV policy should NOT add _HEVC suffix, got {mov_name}"
        );
    }

    // ── expected_paths_for negative-space coverage ───────────────────────
    //
    // The 11 happy-path expected_paths_* tests above leave a lot of input
    // surface untested. These pin behavior on the filename / album-name
    // edges most likely to surprise: non-ASCII when keep_unicode is on vs
    // off, traversal-style names, names that vanish after sanitization,
    // separators inside filenames, and weird album names.

    #[test]
    fn expected_paths_keeps_unicode_when_flag_set() {
        let asset = TestPhotoAsset::new("UNI_1")
            .filename("héllo_wörld.JPG")
            .build();
        let mut config = test_config();
        config.keep_unicode_in_filenames = true;
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            name.contains('é') && name.contains('ö'),
            "keep_unicode=true should preserve non-ASCII, got {name}"
        );
    }

    #[test]
    fn expected_paths_strips_unicode_when_flag_off() {
        let asset = TestPhotoAsset::new("UNI_2")
            .filename("héllo_wörld.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains('é') && !name.contains('ö') && name.contains("hllo_wrld"),
            "keep_unicode=false should strip non-ASCII, got {name}"
        );
    }

    #[test]
    fn expected_paths_filename_emptied_by_unicode_strip_uses_fingerprint() {
        let asset = TestPhotoAsset::new("UNI_3").filename("日本語.jpg").build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            name,
            paths::generate_fingerprint_filename("UNI_3", "public.jpeg"),
            "all-non-ASCII stem must fall back to a visible fingerprint name"
        );
    }

    #[test]
    fn expected_paths_filename_without_extension_emptied_by_unicode_strip_uses_fingerprint() {
        let asset = TestPhotoAsset::new("UNI_NOEXT").filename("日本語").build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            name,
            paths::generate_fingerprint_filename("UNI_NOEXT", "public.jpeg"),
            "all-non-ASCII filename without an extension must use UTI-derived fingerprint name"
        );
    }

    #[test]
    fn expected_paths_keep_unicode_with_decomposed_form() {
        // NFC "é" (U+00E9) vs NFD "e\u{0301}" — kei does no normalization,
        // so both round-trip when keep_unicode=true. Pin that so a future
        // unicode-normalization pass doesn't silently change matches.
        let nfc = "ca\u{00e9}.JPG";
        let nfd = "cae\u{0301}.JPG";
        let mut config = test_config();
        config.keep_unicode_in_filenames = true;
        for (label, fname) in [("NFC", nfc), ("NFD", nfd)] {
            let asset = TestPhotoAsset::new("UNI_4").filename(fname).build();
            let paths = expected_paths_for(&asset, &config);
            assert_eq!(paths.len(), 1, "{label}: expected one path");
            let name = paths[0].path.file_name().unwrap().to_string_lossy();
            assert_eq!(
                name, fname,
                "{label}: filename round-trip should be byte-identical"
            );
        }
    }

    #[test]
    fn expected_paths_filename_with_path_separators_is_safe() {
        // iCloud filenames shouldn't contain `/` but the wire format is
        // a string, so a malformed asset could carry one. The path must
        // still be confined to `directory` (no traversal out).
        let asset = TestPhotoAsset::new("SEP_1")
            .filename("evil/IMG.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let path_str = paths[0].path.to_string_lossy().into_owned();
        // The directory prefix is stable; everything after must not
        // re-introduce a `/IMG` segment that could escape into a sibling
        // directory.
        let dir_str = config.directory.to_string_lossy().into_owned();
        assert!(
            path_str.starts_with(&dir_str),
            "path escaped directory: {path_str}"
        );
        let suffix = path_str.trim_start_matches(&*dir_str);
        assert!(
            !suffix.contains("/evil/") && !suffix.contains("evil/IMG.JPG"),
            "raw `evil/IMG.JPG` survived sanitization: {suffix}"
        );
    }

    #[test]
    fn expected_paths_filename_with_traversal_is_safe() {
        // `../../etc/passwd.JPG` — the path-separator + traversal
        // sequence has to land inside `directory`, not at /etc/passwd.
        // Sanitization replaces `/` with `_`, so `..` substrings can
        // survive *as part of one filename*, which is harmless. What
        // must NOT happen: the path having extra segments that walk
        // out of `directory`.
        let asset = TestPhotoAsset::new("TRAV_1")
            .filename("../../etc/passwd.JPG")
            .build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let path = &paths[0].path;
        assert!(
            path.starts_with(&*config.directory),
            "path escaped configured directory: {}",
            path.display()
        );
        // Folder template is `%Y/%m/%d` (3 dated dirs) + 1 filename
        // = 4 components past `directory`. Anything more means a
        // traversal segment leaked into the path tree.
        let suffix = path.strip_prefix(&*config.directory).unwrap();
        assert_eq!(
            suffix.components().count(),
            4,
            "extra path segments (traversal leak): {}",
            suffix.display()
        );
        // And the literal `/etc/passwd` must not appear as part of a
        // path component sequence.
        let path_str = path.to_string_lossy();
        assert!(
            !path_str.contains("/etc/") && !path_str.contains("/passwd."),
            "raw traversal segments survived in the path: {path_str}"
        );
    }

    #[test]
    fn expected_paths_album_name_with_separators_sanitized() {
        let asset = TestPhotoAsset::new("ALB_1")
            .filename("IMG_0001.JPG")
            .build();
        let mut config = test_config();
        config.folder_structure = "{album}".to_string();
        config.album_name = Some(Arc::from("evil/../escape"));
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let path_str = paths[0].path.to_string_lossy().into_owned();
        assert!(
            !path_str.contains("..") && !path_str.contains("/escape/"),
            "album traversal survived: {path_str}"
        );
    }

    #[test]
    fn expected_paths_filename_only_dots_and_spaces() {
        // "  ...  " trims to empty — filename derivation has to produce
        // *some* name, not a literal "" segment.
        let asset = TestPhotoAsset::new("DOTS_1").filename("  ...  ").build();
        let config = test_config();
        let paths = expected_paths_for(&asset, &config);
        assert_eq!(paths.len(), 1);
        let name = paths[0].path.file_name().unwrap().to_string_lossy();
        assert!(
            !name.is_empty() && !name.trim_matches(|c: char| c == '.' || c == ' ').is_empty(),
            "filename must not collapse to a dots/spaces-only name, got {name:?}"
        );
    }

    #[test]
    fn resolution_none_live_edited_keeps_import_and_sync_mov_name_in_parity() {
        let asset = TestPhotoAsset::new("PR4_NONE_LIVE_EDITED")
            .filename("IMG_NONE_LIVE.HEIC")
            .item_type("public.heic")
            .orig_file_type("public.heic")
            .adjusted_version(
                "https://p01.icloud-content.com/edited_image",
                "edited_image_ck",
                900,
                "public.heic",
            )
            .live_adjusted(
                "https://p01.icloud-content.com/edited_mov",
                "edited_mov_ck",
                2500,
            )
            .live_photo("https://p01.icloud-content.com/live_mov", "mov_ck", 3000)
            .build();
        let mut config = test_config();
        config.resolution = crate::types::PhotoResolution::None;
        config.edited = true;

        let expected = expected_paths_for(&asset, &config);
        let tasks = filter_asset_fresh(&asset, &config);
        let expected_mov = expected
            .iter()
            .find(|p| p.version_size == VersionSizeKey::LiveOriginal)
            .expect("expected import MOV path");
        let task_mov = tasks
            .iter()
            .find(|t| t.version_size == VersionSizeKey::LiveOriginal)
            .expect("expected sync MOV task");

        let expected_name = expected_mov.path.file_name().unwrap().to_string_lossy();
        let task_name = task_mov
            .download_path
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert_eq!(
            expected_name, task_name,
            "import-existing and sync must agree on MOV filename when primary is disabled"
        );
        assert!(
            !expected_name.contains("_edited"),
            "original MOV must not inherit the edited still filename: {expected_name}"
        );
    }
}
