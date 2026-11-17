//! Shared path and filter configuration for sync and import.

use std::path::Path;
use std::sync::Arc;

use rustc_hash::FxHashSet;

use crate::download::{DownloadConfig, paths};
use crate::types::{FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, RawPolicy};

pub(crate) trait PathDerivationSource {
    fn album_bound_parent(
        &self,
        _library: Option<&str>,
        _child: &str,
    ) -> Option<std::path::PathBuf> {
        None
    }
    fn album_bound_path(
        &self,
        _library: Option<&str>,
        _child: &str,
        _version: &str,
    ) -> Option<std::path::PathBuf> {
        None
    }
    fn directory(&self) -> &Path;
    fn folder_structure(&self) -> &str;
    fn media(&self) -> &crate::config::MediaSelection;
    fn skip_created_before(&self) -> Option<crate::config::CreatedDateFilter>;
    fn skip_created_after(&self) -> Option<crate::config::CreatedDateFilter>;
    fn live_photo_mode(&self) -> LivePhotoMode;
    fn live_photo_mov_filename_policy(&self) -> LivePhotoMovFilenamePolicy;
    fn edited_naming(&self) -> crate::types::EditedNaming;
    fn raw_policy(&self) -> RawPolicy;
    fn file_match_policy(&self) -> FileMatchPolicy;
    fn keep_unicode_in_filenames(&self) -> bool;
    fn album_name(&self) -> Option<&str>;
    fn exclude_asset_ids(&self) -> &FxHashSet<String>;
}

/// Path/filter settings needed to derive where sync would place an asset.
///
/// `import-existing` uses this instead of constructing a mostly inert
/// [`DownloadConfig`], so path matching cannot accidentally depend on
/// pipeline-only defaults like retry policy, state DB handles, concurrency,
/// sync mode, or bandwidth limiters.
#[derive(Debug, Clone)]
pub(crate) struct PathDerivationConfig {
    pub(crate) directory: Arc<Path>,
    pub(crate) folder_structure: String,
    pub(crate) folder_structure_albums: Arc<str>,
    pub(crate) folder_structure_smart_folders: Arc<str>,
    pub(crate) media: crate::config::MediaSelection,
    pub(crate) skip_created_before: Option<crate::config::CreatedDateFilter>,
    pub(crate) skip_created_after: Option<crate::config::CreatedDateFilter>,
    pub(crate) live_photo_mode: LivePhotoMode,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) edited_naming: crate::types::EditedNaming,
    pub(crate) raw_policy: RawPolicy,
    pub(crate) file_match_policy: FileMatchPolicy,
    pub(crate) keep_unicode_in_filenames: bool,
    pub(crate) album_name: Option<Arc<str>>,
    pub(crate) library: Arc<str>,
    pub(crate) exclude_asset_ids: Arc<FxHashSet<String>>,
}

impl PathDerivationConfig {
    pub(crate) fn from_path_fields(
        directory: Arc<Path>,
        fields: crate::config::PathDerivationFields,
        media: crate::config::MediaSelection,
    ) -> Self {
        Self {
            directory,
            folder_structure: fields.folder_structure,
            folder_structure_albums: Arc::from(fields.folder_structure_albums.as_str()),
            folder_structure_smart_folders: Arc::from(
                fields.folder_structure_smart_folders.as_str(),
            ),
            media,
            skip_created_before: None,
            skip_created_after: None,
            live_photo_mode: fields.live_photo_mode,
            live_photo_mov_filename_policy: fields.live_photo_mov_filename_policy,
            edited_naming: fields.edited_naming,
            raw_policy: fields.raw_policy,
            file_match_policy: fields.file_match_policy,
            keep_unicode_in_filenames: fields.keep_unicode_in_filenames,
            album_name: None,
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
            exclude_asset_ids: Arc::new(FxHashSet::default()),
        }
    }

    /// Clone this path config for one resolved album/smart-folder/unfiled
    /// pass, mirroring [`DownloadConfig::with_pass`] without pulling in
    /// download-pipeline fields.
    pub(crate) fn with_pass(&self, pass: &crate::commands::AlbumPass) -> Self {
        let folder_structure = folder_structure_for_pass(
            &self.folder_structure,
            &self.folder_structure_albums,
            &self.folder_structure_smart_folders,
            &self.library,
            pass,
        );
        Self {
            album_name: Some(Arc::clone(&pass.album.name)),
            folder_structure,
            exclude_asset_ids: Arc::clone(&pass.exclude_ids),
            ..self.clone()
        }
    }

    /// Clone this config with a different CloudKit zone for `{library}` path
    /// expansion.
    pub(crate) fn with_library(&self, library: &str) -> Self {
        Self {
            library: Arc::from(library),
            ..self.clone()
        }
    }
}

/// Pick and expand the active template for one album/smart-folder/unfiled
/// pass. Shared by sync's [`DownloadConfig`] and import's
/// [`PathDerivationConfig`] so `{album}`, `{smart-folder}`, and `{library}`
/// expansion cannot drift between matching and downloading.
pub(in crate::download) fn folder_structure_for_pass(
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
    library: &str,
    pass: &crate::commands::AlbumPass,
) -> String {
    let template: &str = match pass.kind {
        crate::commands::PassKind::Album => folder_structure_albums,
        crate::commands::PassKind::SmartFolder => folder_structure_smart_folders,
        crate::commands::PassKind::Unfiled => folder_structure,
    };
    let name_ref = Some(pass.album.name.as_ref()).filter(|n: &&str| !n.is_empty());
    let category_expanded = if pass.kind == crate::commands::PassKind::Album {
        paths::expand_album_components(template, pass.album.selector_components())
    } else {
        paths::expand_named_token(template, pass.kind.token(), name_ref)
    };
    // Apply `{library}` last with the path-friendly truncated zone name,
    // so callers see `SharedSync-A1B2C3D4/...` instead of the full UUID.
    // The state-DB key still uses the full zone name.
    let library_for_path = paths::truncate_library_zone(library);
    paths::expand_named_token(
        &category_expanded,
        paths::TOKEN_LIBRARY,
        Some(library_for_path),
    )
}

impl PathDerivationSource for PathDerivationConfig {
    fn directory(&self) -> &Path {
        &self.directory
    }

    fn folder_structure(&self) -> &str {
        &self.folder_structure
    }

    fn media(&self) -> &crate::config::MediaSelection {
        &self.media
    }

    fn skip_created_before(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_before
    }

    fn skip_created_after(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_after
    }

    fn live_photo_mode(&self) -> LivePhotoMode {
        self.live_photo_mode
    }

    fn live_photo_mov_filename_policy(&self) -> LivePhotoMovFilenamePolicy {
        self.live_photo_mov_filename_policy
    }

    fn edited_naming(&self) -> crate::types::EditedNaming {
        self.edited_naming
    }

    fn raw_policy(&self) -> RawPolicy {
        self.raw_policy
    }

    fn file_match_policy(&self) -> FileMatchPolicy {
        self.file_match_policy
    }

    fn keep_unicode_in_filenames(&self) -> bool {
        self.keep_unicode_in_filenames
    }

    fn album_name(&self) -> Option<&str> {
        self.album_name.as_deref()
    }

    fn exclude_asset_ids(&self) -> &FxHashSet<String> {
        &self.exclude_asset_ids
    }
}

impl PathDerivationSource for DownloadConfig {
    fn album_bound_parent(&self, library: Option<&str>, child: &str) -> Option<std::path::PathBuf> {
        let container = self.primary_layout_source.as_ref()?.container_id()?;
        self.selection_context.as_ref()?.album_parent(
            container,
            library.unwrap_or(&self.library),
            child,
        )
    }
    fn album_bound_path(
        &self,
        library: Option<&str>,
        child: &str,
        version: &str,
    ) -> Option<std::path::PathBuf> {
        let container = self.primary_layout_source.as_ref()?.container_id()?;
        self.selection_context.as_ref()?.album_file(
            container,
            library.unwrap_or(&self.library),
            child,
            version,
        )
    }
    fn directory(&self) -> &Path {
        &self.directory
    }

    fn folder_structure(&self) -> &str {
        &self.folder_structure
    }

    fn media(&self) -> &crate::config::MediaSelection {
        &self.media
    }

    fn skip_created_before(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_before
    }

    fn skip_created_after(&self) -> Option<crate::config::CreatedDateFilter> {
        self.skip_created_after
    }

    fn live_photo_mode(&self) -> LivePhotoMode {
        self.live_photo_mode
    }

    fn live_photo_mov_filename_policy(&self) -> LivePhotoMovFilenamePolicy {
        self.live_photo_mov_filename_policy
    }

    fn edited_naming(&self) -> crate::types::EditedNaming {
        self.edited_naming
    }

    fn raw_policy(&self) -> RawPolicy {
        self.raw_policy
    }

    fn file_match_policy(&self) -> FileMatchPolicy {
        self.file_match_policy
    }

    fn keep_unicode_in_filenames(&self) -> bool {
        self.keep_unicode_in_filenames
    }

    fn album_name(&self) -> Option<&str> {
        self.album_name.as_deref()
    }

    fn exclude_asset_ids(&self) -> &FxHashSet<String> {
        &self.exclude_asset_ids
    }
}

#[cfg(test)]
mod hierarchy_tests {
    use super::folder_structure_for_pass;
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoAlbum;
    use std::sync::Arc;

    #[test]
    fn album_hierarchy_renders_components_without_inventing_separators() {
        let pass = AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("unused-rendered-name"))
                .with_selector_components(vec![
                    "Family/Work".into(),
                    "..".into(),
                    "100% Trips".into(),
                ]),
            exclude_ids: Arc::default(),
        };
        let template = folder_structure_for_pass("", "{album}/%Y", "", "PrimarySync", &pass);
        assert_eq!(template, "Family_Work/_/100%% Trips/%Y");
        assert_eq!(pass.album.selector_components()[0], "Family/Work");
    }
}
