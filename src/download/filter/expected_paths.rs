//! Bare expected paths shared by sync and import, before collision resolution.

use std::borrow::Cow;
use std::path::PathBuf;

use chrono::{DateTime, FixedOffset};

use crate::download::paths;
use crate::icloud::photos::types::AssetVersion;
use crate::state::VersionSizeKey;
use crate::types::{AssetVersionSize, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy};

use super::config::PathDerivationSource;
use super::versions::VersionsView;
use super::versions::{
    apply_raw_policy, select_alternative_extra, select_edited_extra, select_live_edited_extra,
    select_mov_companion, select_primary,
};

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
    pub(in crate::download) naming_role: NamingRole,
}

/// Placement role is independent of the stable rendition key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum NamingRole {
    #[default]
    Rendition,
    Current,
    OriginalArchive,
}

impl NamingRole {
    pub(in crate::download) fn suffix(self) -> &'static str {
        match self {
            Self::OriginalArchive => "_original",
            Self::Current | Self::Rendition => "",
        }
    }
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

fn live_photo_motion_filename_for_primary(
    primary_filename: &str,
    config: &(impl PathDerivationSource + ?Sized),
) -> String {
    match config.live_photo_mov_filename_policy() {
        crate::types::LivePhotoMovFilenamePolicy::Suffix => {
            paths::live_photo_mov_path_suffix(primary_filename)
        }
        crate::types::LivePhotoMovFilenamePolicy::Original => {
            paths::live_photo_mov_path_original(primary_filename)
        }
    }
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
        naming_role: NamingRole::Rendition,
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
        naming_role: NamingRole::Rendition,
    }
}

pub(in crate::download) fn derive_edited_extra(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    seen_urls: &[Box<str>],
) -> Option<DerivedPath> {
    let (version, key) = select_edited_extra(asset, config, &ctx.versions, &[])?;
    if boxed_url_seen(version, seen_urls) {
        return None;
    }
    Some(derive_suffixed_extra(
        asset, config, ctx, version, key, "_edited",
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
        asset, config, ctx, version, key, suffix,
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
        asset, config, ctx, version, key, "_edited",
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
        naming_role: NamingRole::Rendition,
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
            return apply_album_binding(
                asset,
                config,
                apply_primary_naming(asset, config, &ctx, out),
            );
        }
        out.push(mov);
    }
    apply_album_binding(
        asset,
        config,
        apply_primary_naming(asset, config, &ctx, out),
    )
}

fn apply_album_binding(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    mut paths: Vec<DerivedPath>,
) -> Vec<DerivedPath> {
    for path in &mut paths {
        apply_album_path(asset, config, path);
    }
    paths
}

pub(super) fn apply_album_path(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    path: &mut DerivedPath,
) {
    if let Some(bound) = config.album_bound_path(
        asset.source_zone(),
        asset.state_id(),
        path.version_size.as_str(),
    ) {
        if config.edited_naming() == crate::types::EditedNaming::Primary {
            // The active primary policy owns rendition names and promotion.
            // Album routing supplies only the established parent directory.
            path.path = bound.with_file_name(&path.filename);
            return;
        }
        if let Some(name) = bound.file_name().and_then(|name| name.to_str()) {
            path.filename = name.to_owned();
        }
        path.path = bound;
    } else if let Some(parent) = config.album_bound_parent(asset.source_zone(), asset.state_id()) {
        path.path = parent.join(&path.filename);
    }
}

fn apply_primary_naming(
    asset: &crate::icloud::photos::PhotoAsset,
    config: &(impl PathDerivationSource + ?Sized),
    ctx: &DerivationContext<'_>,
    mut paths: Vec<DerivedPath>,
) -> Vec<DerivedPath> {
    if config.edited_naming() != crate::types::EditedNaming::Primary {
        return paths;
    }
    let adjusted_still = paths
        .iter()
        .any(|p| p.version_size == VersionSizeKey::Adjusted);
    let adjusted_motion = paths
        .iter()
        .any(|p| p.version_size == VersionSizeKey::LiveAdjusted);
    let video_only = config.live_photo_mode() == LivePhotoMode::VideoOnly;
    for p in &mut paths {
        let key = p.version_size;
        let role = match key {
            VersionSizeKey::Adjusted => NamingRole::Current,
            VersionSizeKey::Original if adjusted_still => NamingRole::OriginalArchive,
            VersionSizeKey::Original => NamingRole::Current,
            VersionSizeKey::LiveOriginal if adjusted_still || (video_only && adjusted_motion) => {
                NamingRole::OriginalArchive
            }
            VersionSizeKey::LiveOriginal => NamingRole::Current,
            VersionSizeKey::LiveAdjusted if adjusted_still || video_only => NamingRole::Current,
            _ => NamingRole::Rendition,
        };
        if role == NamingRole::Rendition {
            continue;
        }
        // Derive from the provider stem, never from a filename with an inferred
        // role. This preserves literal provider names ending in _edited/_original.
        let base = if key == VersionSizeKey::LiveOriginal || key == VersionSizeKey::LiveAdjusted {
            let base = usable_asset_base_filename(asset, ctx);
            let base = live_photo_motion_filename_for_primary(&base, config);
            let version = ctx.versions.get(if key == VersionSizeKey::LiveAdjusted {
                AssetVersionSize::LiveAdjusted
            } else {
                AssetVersionSize::LiveOriginal
            });
            let Some(version) = version else {
                continue;
            };
            mapped_version_filename(asset.state_id(), &base, &version.asset_type)
        } else {
            let version = ctx.versions.get(match key {
                VersionSizeKey::Adjusted => AssetVersionSize::Adjusted,
                _ => AssetVersionSize::Original,
            });
            let Some(version) = version else {
                continue;
            };
            mapped_version_filename(asset.state_id(), &ctx.base_filename, &version.asset_type)
        };
        let base = match config.file_match_policy() {
            FileMatchPolicy::NameId7 => paths::apply_name_id7(&base, asset.state_id()),
            FileMatchPolicy::NameSizeDedupWithSuffix => base,
        };
        p.filename = paths::role_filename(&base, "", role.suffix());
        p.path = paths::local_download_path(
            config.directory(),
            config.folder_structure(),
            &ctx.created_local,
            &p.filename,
            config.album_name(),
        );
        p.naming_role = role;
    }
    paths
}
