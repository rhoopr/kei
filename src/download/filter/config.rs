//! Shared path and filter configuration for sync and import.

use std::path::Path;
use std::sync::Arc;

use rustc_hash::FxHashSet;

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
