//! Library-scoped state snapshots and existing asset identity selection.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::download::filter::{FilterReason, is_asset_filtered};
use crate::icloud::photos::PhotoAsset;
use crate::state::{
    DownloadContextStateStore, DownloadStateStore, MetadataRewriteStore, VersionSizeKey,
};

use super::config::DownloadConfig;
use super::models::DownloadStore;

/// Look up an owned `String` id in a shared `Arc<str>` interner,
/// inserting a fresh `Arc` if absent. Returns the shared handle.
///
/// `Arc::from(String)` transfers the string's buffer into the `Arc<str>`
/// without copying the bytes, so the miss path allocates no more than
/// the baseline cost of a fresh handle.
fn intern_id(interner: &mut FxHashSet<Arc<str>>, s: String) -> Arc<str> {
    if let Some(existing) = interner.get(s.as_str()) {
        return Arc::clone(existing);
    }
    let a: Arc<str> = Arc::from(s);
    interner.insert(Arc::clone(&a));
    a
}

/// Pre-loaded download state for O(1) skip decisions.
///
/// Loaded once at sync start from the state database, this enables fast
/// in-memory lookups instead of per-asset DB queries. For 100K+ asset
/// libraries, this significantly reduces DB roundtrips.
///
/// Asset-id keys are `Arc<str>` rather than `Box<str>` so the same id
/// allocation is shared across every map here (and with the producer's
/// seen-ids / touched-ids sets). On a 100k-asset library this collapses
/// ~4-6 independent `Box<str>` allocations per asset into one.
/// `library -> asset_id -> set of version_size` strings. Used by
/// `DownloadContext::downloaded_ids` and `metadata_retry_markers`.
type LibraryAssetVersionSet = FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashSet<Box<str>>>>;

/// `library -> asset_id`. Used by retry-only mode to decide whether an asset is
/// already known in the same CloudKit zone.
type LibraryAssetSet = FxHashMap<Arc<str>, FxHashSet<Arc<str>>>;

/// `library -> asset_id -> attempt count`. Used by max-attempt gating without
/// leaking failures across libraries that happen to share an id.
type LibraryAssetAttemptCounts = FxHashMap<Arc<str>, FxHashMap<Arc<str>, u32>>;

/// `library -> asset_id -> (version_size -> value)`. Used by
/// `DownloadContext::downloaded_checksums` and
/// `downloaded_metadata_hashes`.
type LibraryAssetVersionValueMap =
    FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashMap<Box<str>, Box<str>>>>;

/// Durable local-file evidence for a state-backed skip or pending adoption.
#[derive(Debug)]
pub(in crate::download) struct RecordedLocalFile {
    pub(in crate::download) path: PathBuf,
    pub(in crate::download) local_checksum: Option<Box<str>>,
    pub(in crate::download) download_checksum: Option<Box<str>>,
}

/// `library -> asset_id -> (version_size -> recorded local-file evidence)`.
type LibraryAssetVersionFileMap =
    FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashMap<Box<str>, RecordedLocalFile>>>;

/// `library -> master_record_name -> asset_record_names`. Full enumeration
/// uses this durable family history to keep a legacy master-keyed state row
/// attached to the same child when CloudKit changes record order or page
/// boundaries.
type LibraryMasterAssetSet = FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashSet<Arc<str>>>>;

/// `library -> master_record_name -> asset_record_name`. The value is the
/// durable child that owns a legacy master-keyed state row.
type LibraryMasterAssetMap = FxHashMap<Arc<str>, FxHashMap<Arc<str>, Arc<str>>>;

pub(in crate::download) type ClaimedLegacyMasterStates = FxHashSet<(Arc<str>, Arc<str>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LegacyOwnerClaimMode {
    ExistingOnly,
    ReadOnly,
    Persist,
}

/// Prevent a child excluded by every pass's album membership from claiming a
/// mapping-less legacy row. Other filters still select identity because they
/// apply to metadata refreshes even when no media task is planned.
pub(super) fn legacy_owner_claim_mode_for_configs<'a>(
    requested: LegacyOwnerClaimMode,
    asset: &PhotoAsset,
    configs: impl IntoIterator<Item = &'a DownloadConfig>,
) -> LegacyOwnerClaimMode {
    if requested == LegacyOwnerClaimMode::ExistingOnly
        || configs.into_iter().all(|config| {
            matches!(
                is_asset_filtered(asset, config),
                Some(FilterReason::ExcludedAlbum)
            )
        })
    {
        LegacyOwnerClaimMode::ExistingOnly
    } else {
        requested
    }
}

const LEGACY_OWNER_CLAIM_MAX_ATTEMPTS: u32 = 3;

const LEGACY_OWNER_CLAIM_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
pub(in crate::download) struct DownloadContext {
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` that
    /// are already downloaded. Three-level shape so multi-library syncs
    /// don't dedupe the same asset_id across zones (PR10 / schema v8).
    /// All key levels use borrowed `&str` lookups for zero-allocation probes.
    pub(in crate::download) downloaded_ids: LibraryAssetVersionSet,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> checksum).
    /// Used to detect checksum changes (CloudKit asset updated) without DB queries.
    pub(in crate::download) downloaded_checksums: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> local file).
    /// Used to validate path-aware filesystem skips after state says the
    /// remote bytes are unchanged.
    pub(in crate::download) downloaded_files: LibraryAssetVersionFileMap,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> metadata_hash).
    /// Used to detect metadata-only changes (favorite toggle, keywords, GPS
    /// edit, etc.) when file bytes are unchanged but CloudKit has newer
    /// metadata.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(in crate::download) downloaded_metadata_hashes: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` with a
    /// non-null `metadata_write_failed_at` from a prior sync. These always
    /// route to the metadata-rewrite path regardless of whether the hash
    /// changed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(in crate::download) metadata_retry_markers: LibraryAssetVersionSet,
    /// Assets the provider reported deleted whose downloaded rows remain.
    /// Staleness checks skip these so they cannot report a change that
    /// `refresh_downloaded_asset_metadata` is unable to apply.
    pub(in crate::download) soft_deleted_ids: LibraryAssetSet,
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` that
    /// are pending at sync start. Used to resolve failed/pending rows when
    /// the expected file is already on disk instead of promoting them back to
    /// failed after the producer skips the duplicate path.
    pub(in crate::download) pending_ids: LibraryAssetVersionSet,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> filename)
    /// for pending rows. Pending on-disk adoption uses this to avoid adopting
    /// a same-name/same-size collision that belongs to a different asset.
    pub(in crate::download) pending_filenames: LibraryAssetVersionValueMap,
    /// Provider checksums for pending rows. Identity stabilization uses these
    /// to retain a compatible legacy master-keyed retry without merging a
    /// different sibling into it.
    pub(in crate::download) pending_checksums: LibraryAssetVersionValueMap,
    /// Current recorded local files for pending rows. A provider checksum
    /// change clears `downloaded_at`, so historical paths are excluded.
    pub(in crate::download) pending_files: LibraryAssetVersionFileMap,
    /// Provider identity families recorded before this pipeline started.
    /// `None` disables legacy adoption after a mapping query failure. The
    /// snapshot intentionally excludes mappings discovered during the current
    /// run so a newly seen sibling cannot claim another child's legacy state.
    pub(in crate::download) asset_master_mappings: Option<LibraryMasterAssetSet>,
    /// Persisted child owner for each adopted legacy master-keyed state row.
    /// `None` disables new legacy adoption after an owner query failure.
    pub(in crate::download) legacy_master_state_owners: Option<LibraryMasterAssetMap>,
    /// Nested map: `library` -> set of asset IDs known to the state DB (any
    /// status). Used in retry-only mode to skip new assets that were never
    /// synced in the same CloudKit zone.
    pub(in crate::download) known_ids: LibraryAssetSet,
    /// Per-library/asset maximum download attempt count (from failed assets).
    /// Used to skip assets that have exceeded `max_download_attempts`.
    pub(in crate::download) attempt_counts: LibraryAssetAttemptCounts,
    /// True when at least one downloaded asset-version lacks a metadata hash.
    /// Cached because the producer checks this on hot on-disk skip paths.
    pub(in crate::download) downloaded_without_metadata_hash: bool,
}

impl DownloadContext {
    /// Load the download context from the state database. All state queries
    /// are independent and run concurrently so sync start doesn't serialize
    /// on round-trip latency across them.
    pub(in crate::download) async fn load<D>(db: &D, retry_only: bool) -> Self
    where
        D: DownloadContextStateStore + DownloadStateStore + MetadataRewriteStore + ?Sized,
    {
        let known_ids_fut = async {
            if retry_only {
                db.get_all_known_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load known IDs from state DB");
                    Default::default()
                })
            } else {
                Default::default()
            }
        };
        let (
            downloaded_records,
            soft_deleted,
            hashes,
            markers,
            pending,
            attempts,
            known_id_rows,
            mapping_rows,
            legacy_owner_rows,
        ) = tokio::join!(
            async {
                db.get_downloaded_file_records().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load downloaded records from state DB");
                    Default::default()
                })
            },
            async {
                db.get_soft_deleted_downloaded_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load soft-deleted assets from state DB");
                    Default::default()
                })
            },
            async {
                db.get_downloaded_metadata_hashes()
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "Failed to load metadata hashes from state DB");
                        Default::default()
                    })
            },
            async {
                db.get_metadata_retry_markers().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load metadata retry markers from state DB");
                    Default::default()
                })
            },
            async {
                db.get_pending().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load pending assets from state DB");
                    Default::default()
                })
            },
            async {
                db.get_attempt_counts().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load attempt counts from state DB");
                    Default::default()
                })
            },
            known_ids_fut,
            db.get_asset_master_mappings(),
            db.get_legacy_master_state_owners(),
        );

        // Shared interner so the same asset_id allocates exactly one
        // `Arc<str>` across every map below (and is cheaply cloneable
        // into each via Arc::clone). This collapses the former 4-6
        // independent `String -> Box<str>` conversions per id into one.
        //
        // FxHashSet<Arc<str>> over FxHashMap<String, Arc<str>> so the
        // interner doesn't keep a duplicate `String` alive for every
        // id; `Arc::from(String)` transfers the String's buffer into
        // the Arc without an extra copy.
        let mut interner: FxHashSet<Arc<str>> = FxHashSet::default();

        let mut downloaded_ids: LibraryAssetVersionSet = FxHashMap::default();
        let mut downloaded_checksums: LibraryAssetVersionValueMap = FxHashMap::default();
        let mut downloaded_files: LibraryAssetVersionFileMap = FxHashMap::default();
        for record in downloaded_records {
            let crate::state::DownloadedFileRecord {
                library,
                id,
                version_size,
                checksum,
                local_path,
                local_checksum,
                download_checksum,
            } = record;
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, id);
            let version_size: Box<str> = version_size.as_str().into();
            downloaded_ids
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone());
            downloaded_checksums
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone(), checksum.into_boxed_str());
            if let Some(path) = local_path {
                downloaded_files
                    .entry(lib)
                    .or_default()
                    .entry(id)
                    .or_default()
                    .insert(
                        version_size,
                        RecordedLocalFile {
                            path,
                            local_checksum: local_checksum.map(String::into_boxed_str),
                            download_checksum: download_checksum.map(String::into_boxed_str),
                        },
                    );
            }
        }

        let mut soft_deleted_ids: LibraryAssetSet = FxHashMap::default();
        for (library, asset_id) in soft_deleted {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            soft_deleted_ids.entry(lib).or_default().insert(id);
        }

        let mut downloaded_metadata_hashes: LibraryAssetVersionValueMap = FxHashMap::default();
        for ((library, asset_id, version_size), metadata_hash) in hashes {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_metadata_hashes
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(
                    version_size.into_boxed_str(),
                    metadata_hash.into_boxed_str(),
                );
        }

        let mut metadata_retry_markers: LibraryAssetVersionSet = FxHashMap::default();
        for (library, asset_id, version_size) in markers {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            metadata_retry_markers
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let mut pending_ids: LibraryAssetVersionSet = FxHashMap::default();
        let mut pending_filenames: LibraryAssetVersionValueMap = FxHashMap::default();
        let mut pending_checksums: LibraryAssetVersionValueMap = FxHashMap::default();
        let mut pending_files: LibraryAssetVersionFileMap = FxHashMap::default();
        for record in pending {
            let crate::state::AssetRecord {
                library,
                id,
                version_size,
                checksum,
                filename,
                local_path,
                local_checksum,
                download_checksum,
                downloaded_at,
                ..
            } = record;
            let lib = intern_id(&mut interner, library.to_string());
            let id = intern_id(&mut interner, id.to_string());
            let version_size: Box<str> = version_size.as_str().into();
            pending_ids
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone());
            pending_filenames
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone(), filename);
            pending_checksums
                .entry(Arc::clone(&lib))
                .or_default()
                .entry(Arc::clone(&id))
                .or_default()
                .insert(version_size.clone(), checksum);
            if downloaded_at.is_some()
                && let Some(path) = local_path
            {
                pending_files
                    .entry(lib)
                    .or_default()
                    .entry(id)
                    .or_default()
                    .insert(
                        version_size,
                        RecordedLocalFile {
                            path,
                            local_checksum: local_checksum.map(String::into_boxed_str),
                            download_checksum: download_checksum.map(String::into_boxed_str),
                        },
                    );
            }
        }

        let asset_master_mappings = mapping_rows
            .map(|mapping_rows| {
                let mut mappings: LibraryMasterAssetSet = FxHashMap::default();
                for (library, asset_record_name, master_record_name) in mapping_rows {
                    let lib = intern_id(&mut interner, library);
                    let asset = intern_id(&mut interner, asset_record_name);
                    let master = intern_id(&mut interner, master_record_name);
                    mappings
                        .entry(lib)
                        .or_default()
                        .entry(master)
                        .or_default()
                        .insert(asset);
                }
                mappings
            })
            .map_err(|e| {
                tracing::warn!(error = %e, "Failed to load asset/master mappings from state DB");
            })
            .ok();

        let legacy_master_state_owners = legacy_owner_rows
            .map(|owner_rows| {
                let mut owners: LibraryMasterAssetMap = FxHashMap::default();
                for (library, master_record_name, asset_record_name) in owner_rows {
                    let lib = intern_id(&mut interner, library);
                    let master = intern_id(&mut interner, master_record_name);
                    let asset = intern_id(&mut interner, asset_record_name);
                    owners.entry(lib).or_default().insert(master, asset);
                }
                owners
            })
            .map_err(|e| {
                tracing::warn!(error = %e, "Failed to load legacy master state owners from state DB");
            })
            .ok();

        let mut known_ids: LibraryAssetSet = FxHashMap::default();
        for (library, asset_id) in known_id_rows {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            known_ids.entry(lib).or_default().insert(id);
        }

        let mut attempt_counts: LibraryAssetAttemptCounts = FxHashMap::default();
        for ((library, asset_id), count) in attempts {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            attempt_counts.entry(lib).or_default().insert(id, count);
        }
        let downloaded_without_metadata_hash = count_version_set_entries(&downloaded_ids)
            > count_value_map_entries(&downloaded_metadata_hashes);

        Self {
            downloaded_ids,
            downloaded_checksums,
            downloaded_files,
            downloaded_metadata_hashes,
            metadata_retry_markers,
            soft_deleted_ids,
            pending_ids,
            pending_filenames,
            pending_checksums,
            pending_files,
            asset_master_mappings,
            legacy_master_state_owners,
            known_ids,
            attempt_counts,
            downloaded_without_metadata_hash,
        }
    }

    pub(in crate::download) fn is_known(&self, library: &str, asset_id: &str) -> bool {
        self.known_ids
            .get(library)
            .is_some_and(|ids| ids.contains(asset_id))
    }

    pub(in crate::download) fn attempt_count(&self, library: &str, asset_id: &str) -> Option<u32> {
        self.attempt_counts
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .copied()
    }

    /// Whether a downloaded asset-version needs a metadata-only rewrite:
    /// the caller has already matched checksums (bytes unchanged) and now
    /// checks whether (a) the stored metadata_hash differs from the new
    /// one or (b) a persisted retry marker is set from a prior sync where
    /// the writer failed after bytes landed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(in crate::download) fn needs_metadata_rewrite(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        new_metadata_hash: Option<&str>,
    ) -> bool {
        if self.is_soft_deleted(library, asset_id) {
            return false;
        }
        let vs_str = version_size.as_str();
        let has_retry_marker = self
            .metadata_retry_markers
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|vsset| vsset.contains(vs_str));
        if has_retry_marker {
            return true;
        }
        let Some(new_hash) = new_metadata_hash else {
            return false;
        };
        match self
            .downloaded_metadata_hashes
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|map| map.get(vs_str))
        {
            Some(stored) => stored.as_ref() != new_hash,
            None => true, // downloaded row has no stored hash yet -- refresh
        }
    }

    /// Whether the provider reported this asset deleted while its downloaded
    /// rows remain. `refresh_downloaded_asset_metadata` cannot match those
    /// rows, so treating them as stale would report a change that can never
    /// be applied.
    pub(in crate::download) fn is_soft_deleted(&self, library: &str, asset_id: &str) -> bool {
        self.soft_deleted_ids
            .get(library)
            .is_some_and(|assets| assets.contains(asset_id))
    }

    /// Whether at least one downloaded version has provider metadata that
    /// differs from the complete asset returned by iCloud.
    pub(in crate::download) fn has_provider_metadata_drift(
        &self,
        library: &str,
        asset_id: &str,
        capture: &crate::state::MetadataCapture,
    ) -> bool {
        if self.is_soft_deleted(library, asset_id) {
            return false;
        }
        let Some(downloaded_versions) = self
            .downloaded_ids
            .get(library)
            .and_then(|assets| assets.get(asset_id))
        else {
            return false;
        };
        let stored_hashes = self
            .downloaded_metadata_hashes
            .get(library)
            .and_then(|assets| assets.get(asset_id));
        downloaded_versions.iter().any(|version_size| {
            let Some(key) = VersionSizeKey::from_str(version_size) else {
                return true;
            };
            let checksum = self
                .downloaded_checksums
                .get(library)
                .and_then(|assets| assets.get(asset_id))
                .and_then(|versions| versions.get(version_size.as_ref()))
                .map_or("", AsRef::as_ref);
            let metadata = capture.resolve(key, checksum);
            stored_hashes
                .and_then(|hashes| hashes.get(version_size.as_ref()))
                .is_none_or(|stored| Some(stored.as_ref()) != metadata.metadata_hash.as_deref())
        })
    }

    /// Whether any live downloaded version of this asset carries a metadata
    /// rewrite retry marker. Asset-level counterpart to the per-version
    /// `needs_metadata_rewrite`, for callers that decide once per asset.
    pub(in crate::download) fn has_downloaded_metadata_retry_marker(
        &self,
        library: &str,
        asset_id: &str,
    ) -> bool {
        if self.is_soft_deleted(library, asset_id) {
            return false;
        }
        let Some(markers) = self
            .metadata_retry_markers
            .get(library)
            .and_then(|assets| assets.get(asset_id))
        else {
            return false;
        };
        self.downloaded_ids
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .is_some_and(|downloaded| {
                markers
                    .iter()
                    .any(|version_size| downloaded.contains(version_size.as_ref()))
            })
    }

    pub(in crate::download) fn has_state_version(&self, library: &str, asset_id: &str) -> bool {
        self.downloaded_ids
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .is_some_and(|versions| !versions.is_empty())
            || self
                .pending_ids
                .get(library)
                .and_then(|assets| assets.get(asset_id))
                .is_some_and(|versions| !versions.is_empty())
    }

    pub(in crate::download) fn has_matching_downloaded_checksum(
        &self,
        library: &str,
        asset_id: &str,
        asset: &PhotoAsset,
    ) -> bool {
        let Some(checksums) = self
            .downloaded_checksums
            .get(library)
            .and_then(|assets| assets.get(asset_id))
        else {
            return false;
        };
        asset.versions().iter().any(|(version_size, version)| {
            checksums
                .get(VersionSizeKey::from(*version_size).as_str())
                .is_some_and(|stored| stored.as_ref() == version.checksum.as_ref())
        })
    }

    pub(in crate::download) fn has_matching_state_checksum(
        &self,
        library: &str,
        asset_id: &str,
        asset: &PhotoAsset,
    ) -> bool {
        self.has_matching_downloaded_checksum(library, asset_id, asset)
            || self
                .pending_checksums
                .get(library)
                .and_then(|assets| assets.get(asset_id))
                .is_some_and(|checksums| {
                    asset.versions().iter().any(|(version_size, version)| {
                        checksums
                            .get(VersionSizeKey::from(*version_size).as_str())
                            .is_some_and(|stored| stored.as_ref() == version.checksum.as_ref())
                    })
                })
    }

    /// Select the durable local state identity for a provider asset.
    ///
    /// New assets use their unique `CPLAsset.recordName`. A pre-v0.24 row can
    /// still be keyed by `CPLMaster.recordName`; retain that key only when its
    /// provider checksum matches and durable family history identifies this
    /// child as the sole sibling without its own state row.
    pub(in crate::download) fn should_use_legacy_master_state(
        &self,
        library: &str,
        asset: &PhotoAsset,
    ) -> bool {
        let asset_record_name = asset.asset_record_name();
        let master_record_name = asset.id();
        if asset_record_name == master_record_name
            || self.has_state_version(library, asset_record_name)
        {
            return false;
        }
        if !self.has_matching_state_checksum(library, master_record_name, asset) {
            return false;
        }

        let Some(owners) = &self.legacy_master_state_owners else {
            return false;
        };
        if let Some(owner) = owners
            .get(library)
            .and_then(|masters| masters.get(master_record_name))
        {
            return owner.as_ref() == asset_record_name;
        }

        let Some(mappings) = &self.asset_master_mappings else {
            return false;
        };
        let Some(mapped_children) = mappings
            .get(library)
            .and_then(|masters| masters.get(master_record_name))
        else {
            // Databases predating durable asset/master mappings can still
            // retain a checksum-compatible master row for their first child.
            return true;
        };
        let mut children_without_state = mapped_children
            .iter()
            .filter(|child| !self.has_state_version(library, child));
        let sole_child = children_without_state.next();
        children_without_state.next().is_none()
            && sole_child.is_some_and(|child| child.as_ref() == asset_record_name)
    }

    pub(in crate::download) fn select_asset_state_record_name(
        &self,
        library: &str,
        asset: &PhotoAsset,
        claimed_legacy_master_states: &mut ClaimedLegacyMasterStates,
    ) -> Arc<str> {
        let use_legacy_master = self.should_use_legacy_master_state(library, asset)
            && claimed_legacy_master_states.insert((Arc::from(library), Arc::from(asset.id())));
        if use_legacy_master {
            Arc::from(asset.id())
        } else {
            asset.asset_record_name_arc()
        }
    }

    pub(in crate::download) fn select_existing_asset_state_record_name(
        &self,
        library: &str,
        asset: &PhotoAsset,
    ) -> Arc<str> {
        let has_persisted_owner = self
            .legacy_master_state_owners
            .as_ref()
            .and_then(|libraries| libraries.get(library))
            .and_then(|masters| masters.get(asset.id()))
            .is_some();
        if has_persisted_owner && self.should_use_legacy_master_state(library, asset) {
            Arc::from(asset.id())
        } else {
            asset.asset_record_name_arc()
        }
    }

    pub(in crate::download) async fn select_asset_state_record_name_for_download(
        &self,
        db: Option<&dyn DownloadStore>,
        library: &str,
        asset: &PhotoAsset,
        claimed_legacy_master_states: &mut ClaimedLegacyMasterStates,
    ) -> std::result::Result<Arc<str>, crate::state::error::StateError> {
        let state_record_name =
            self.select_asset_state_record_name(library, asset, claimed_legacy_master_states);
        if state_record_name.as_ref() != asset.id()
            || self
                .legacy_master_state_owners
                .as_ref()
                .and_then(|libraries| libraries.get(library))
                .and_then(|masters| masters.get(asset.id()))
                .is_some()
        {
            return Ok(state_record_name);
        }

        let Some(db) = db else {
            return Ok(asset.asset_record_name_arc());
        };

        for attempt in 1..=LEGACY_OWNER_CLAIM_MAX_ATTEMPTS {
            match db
                .claim_legacy_master_state_owner(library, asset.id(), asset.asset_record_name())
                .await
            {
                Ok(true) => return Ok(state_record_name),
                Ok(false) => return Ok(asset.asset_record_name_arc()),
                Err(error) if attempt < LEGACY_OWNER_CLAIM_MAX_ATTEMPTS => {
                    tracing::debug!(
                        library,
                        master_record_name = asset.id(),
                        asset_record_name = asset.asset_record_name(),
                        attempt,
                        error = %error,
                        "Retrying legacy master state owner claim"
                    );
                    let delay =
                        LEGACY_OWNER_CLAIM_RETRY_DELAY.saturating_mul(1u32 << (attempt - 1));
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }

        Err(crate::state::error::StateError::Invariant {
            operation: "select_asset_state_record_name_for_download",
            detail: "legacy owner claim retry loop ran no attempts".into(),
        })
    }

    /// Check if an asset should be downloaded based on pre-loaded state.
    ///
    /// Returns:
    /// - `Some(true)` — definitely needs download (not in DB or checksum changed)
    /// - `Some(false)` — hard skip, DB confirms downloaded with matching checksum
    ///   (only when `trust_state` is true)
    /// - `None` — downloaded with matching checksum but needs filesystem check
    ///   to confirm file is still on disk (when `trust_state` is false)
    ///
    /// `trust_state=true` skips the filesystem stat: only `--only-print-filenames`
    /// uses it (no side effects, the user just wants to preview). The real-sync
    /// path uses `trust_state=false` — see PR #318 for why.
    ///
    /// Uses borrowed `&str` keys for zero-allocation lookups.
    pub(in crate::download) fn should_download_fast(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        checksum: &str,
        trust_state: bool,
    ) -> Option<bool> {
        if checksum.is_empty() {
            tracing::warn!(
                asset_id,
                version_size = %version_size.as_str(),
                "Empty remote checksum cannot be trusted for skip decisions"
            );
            return Some(true);
        }

        let version_size_str = version_size.as_str();

        // Borrowed `&str` keys at every level — no allocation per probe.
        let is_downloaded = self
            .downloaded_ids
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|versions| versions.contains(version_size_str));

        if !is_downloaded {
            // Not in downloaded set — needs download
            return Some(true);
        }

        // Check if checksum changed (also zero-allocation lookup). Track
        // whether a stored checksum is present at all so we can audit the
        // "no stored checksum" path, which pre-v3 rows fall into.
        let stored_checksum = self
            .downloaded_checksums
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size_str));
        if let Some(stored) = stored_checksum {
            if stored.as_ref() != checksum {
                return Some(true);
            }
        } else {
            // Pre-v3 row with no stored local_checksum. Audit so operators can
            // correlate unexpected "skipped" counts with missing checksum
            // history (the row will gain a checksum on next download).
            tracing::debug!(
                asset_id = asset_id,
                version_size = %version_size_str,
                trust_state = trust_state,
                "no stored checksum for downloaded asset-version; skip decision uses trust_state only"
            );
        }

        if trust_state { Some(false) } else { None }
    }

    pub(in crate::download) fn downloaded_file(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
    ) -> Option<&RecordedLocalFile> {
        self.downloaded_files
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size.as_str()))
    }

    pub(in crate::download) fn pending_file(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
    ) -> Option<&RecordedLocalFile> {
        self.pending_files
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size.as_str()))
    }

    pub(in crate::download) fn pending_file_matching_checksum(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        provider_checksum: &str,
    ) -> Option<&RecordedLocalFile> {
        let stored_checksum = self
            .pending_checksums
            .get(library)
            .and_then(|assets| assets.get(asset_id))
            .and_then(|versions| versions.get(version_size.as_str()))?;
        if stored_checksum.as_ref() != provider_checksum {
            return None;
        }
        self.pending_file(library, asset_id, version_size)
    }

    pub(in crate::download) fn has_downloaded_without_metadata_hash(&self) -> bool {
        self.downloaded_without_metadata_hash
    }
}

fn count_version_set_entries(map: &LibraryAssetVersionSet) -> usize {
    map.values()
        .map(|assets| {
            assets
                .values()
                .map(|versions| versions.len())
                .sum::<usize>()
        })
        .sum()
}

fn count_value_map_entries(map: &LibraryAssetVersionValueMap) -> usize {
    map.values()
        .map(|assets| {
            assets
                .values()
                .map(|versions| versions.len())
                .sum::<usize>()
        })
        .sum()
}

pub(in crate::download) async fn preload_download_context(
    config: &DownloadConfig,
) -> Arc<DownloadContext> {
    let download_ctx = if let Some(db) = &config.state_db {
        tracing::debug!("Pre-loading download state from database");
        DownloadContext::load(db.as_ref(), config.retry_only).await
    } else {
        DownloadContext::default()
    };
    tracing::debug!(
        downloaded_ids = download_ctx.downloaded_ids.len(),
        "Download context loaded"
    );
    Arc::new(download_ctx)
}

pub(super) async fn backfill_asset_master_mappings_from_album_history(db: &dyn DownloadStore) {
    match db
        .backfill_asset_master_mappings_from_album_memberships()
        .await
    {
        Ok(0) => {}
        Ok(inserted) => {
            tracing::info!(
                inserted,
                "Backfilled asset/master mappings from album membership history"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to backfill asset/master mappings from album membership history"
            );
        }
    }
}

#[cfg(test)]
mod tests;
