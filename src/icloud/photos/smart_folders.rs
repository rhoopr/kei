//! Apple's built-in smart albums (Favorites, Videos, etc.) — these exist
//! for every iCloud account and use server-side query filters rather than
//! explicit membership lists.

use std::sync::Arc;

use serde_json::{Value, json};

#[derive(Debug)]
pub(crate) struct FolderDef {
    pub(crate) list_type: &'static str,
    pub(crate) query_filter: Option<Arc<Value>>,
    /// True for folders that the iOS Photos app gates behind a separate UI
    /// (Hidden, Recently Deleted). The `--smart-folder all` sentinel skips
    /// these unless the user explicitly asks for `all-with-sensitive`.
    pub(crate) is_sensitive: bool,
}

pub(crate) fn smart_folder_filter(field: &str, value: &str) -> Value {
    json!([{
        "fieldName": field,
        "comparator": "EQUALS",
        "fieldValue": {"type": "STRING", "value": value}
    }])
}

/// Names of every smart folder kei recognises (10 entries, in
/// declaration order). Used by the wizard's "specific smart folders"
/// hint and by the selection layer's user-vs-smart split.
pub(crate) fn smart_folder_names() -> impl Iterator<Item = &'static str> {
    smart_folders().into_iter().map(|(name, _)| name)
}

/// Names of the smart folders that the iOS Photos app gates behind a
/// separate UI. `--smart-folder all` skips these unless the user opts in
/// via `--smart-folder all-with-sensitive`.
pub(crate) fn sensitive_smart_folder_names() -> impl Iterator<Item = &'static str> {
    smart_folders()
        .into_iter()
        .filter(|(_, def)| def.is_sensitive)
        .map(|(name, _)| name)
}

pub(crate) fn smart_folders() -> Vec<(&'static str, FolderDef)> {
    vec![
        (
            "Time-lapse",
            FolderDef {
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(Arc::new(smart_folder_filter("smartAlbum", "TIMELAPSE"))),
                is_sensitive: false,
            },
        ),
        (
            "Videos",
            FolderDef {
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(Arc::new(smart_folder_filter("smartAlbum", "VIDEO"))),
                is_sensitive: false,
            },
        ),
        (
            "Slo-mo",
            FolderDef {
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(Arc::new(smart_folder_filter("smartAlbum", "SLOMO"))),
                is_sensitive: false,
            },
        ),
        (
            "Bursts",
            FolderDef {
                list_type: "CPLBurstStackAssetAndMasterByAssetDate",
                query_filter: None,
                is_sensitive: false,
            },
        ),
        (
            "Favorites",
            FolderDef {
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(Arc::new(smart_folder_filter("smartAlbum", "FAVORITE"))),
                is_sensitive: false,
            },
        ),
        (
            "Panoramas",
            FolderDef {
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(Arc::new(smart_folder_filter("smartAlbum", "PANORAMA"))),
                is_sensitive: false,
            },
        ),
        (
            "Screenshots",
            FolderDef {
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(Arc::new(smart_folder_filter("smartAlbum", "SCREENSHOT"))),
                is_sensitive: false,
            },
        ),
        (
            "Live",
            FolderDef {
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(Arc::new(smart_folder_filter("smartAlbum", "LIVE"))),
                is_sensitive: false,
            },
        ),
        (
            "Recently Deleted",
            FolderDef {
                list_type: "CPLAssetAndMasterDeletedByExpungedDate",
                query_filter: None,
                is_sensitive: true,
            },
        ),
        (
            "Hidden",
            FolderDef {
                list_type: "CPLAssetAndMasterHiddenByAssetDate",
                query_filter: None,
                is_sensitive: true,
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smart_folders_count() {
        let folders = smart_folders();
        assert_eq!(folders.len(), 10);
    }

    #[test]
    fn test_smart_folders_names() {
        let folders = smart_folders();
        let names: Vec<&str> = folders.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"Favorites"));
        assert!(names.contains(&"Videos"));
        assert!(names.contains(&"Screenshots"));
        assert!(names.contains(&"Live"));
        assert!(names.contains(&"Recently Deleted"));
    }

    #[test]
    fn test_smart_folder_filter_produces_valid_json() {
        let filter = smart_folder_filter("smartAlbum", "FAVORITE");
        let arr = filter.as_array().expect("filter should be array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["fieldName"], "smartAlbum");
        assert_eq!(arr[0]["comparator"], "EQUALS");
        assert_eq!(arr[0]["fieldValue"]["value"], "FAVORITE");
    }

    #[test]
    fn test_bursts_has_no_filter() {
        let folders = smart_folders();
        let bursts = folders.iter().find(|(name, _)| *name == "Bursts").unwrap();
        assert!(bursts.1.query_filter.is_none());
    }

    #[test]
    fn test_favorites_has_filter() {
        let folders = smart_folders();
        let favorites = folders
            .iter()
            .find(|(name, _)| *name == "Favorites")
            .unwrap();
        assert!(favorites.1.query_filter.is_some());
    }

    #[test]
    fn test_hidden_has_no_filter() {
        let folders = smart_folders();
        let hidden = folders.iter().find(|(name, _)| *name == "Hidden").unwrap();
        assert!(hidden.1.query_filter.is_none());
    }

    #[test]
    fn test_recently_deleted_has_no_filter() {
        let folders = smart_folders();
        let deleted = folders
            .iter()
            .find(|(name, _)| *name == "Recently Deleted")
            .unwrap();
        assert!(deleted.1.query_filter.is_none());
    }

    #[test]
    fn test_all_smart_folders_have_unique_names() {
        let folders = smart_folders();
        let mut names: Vec<&str> = folders.iter().map(|(n, _)| *n).collect();
        let len_before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            len_before,
            "Duplicate smart folder names found"
        );
    }

    #[test]
    fn test_all_smart_folders_have_list_types() {
        let folders = smart_folders();
        assert!(!folders.is_empty(), "smart_folders() must not be empty");
        for (name, def) in &folders {
            assert!(!def.list_type.is_empty(), "{name} has empty list_type");
        }
    }

    #[test]
    fn test_smart_folder_filter_field_structure() {
        let filter = smart_folder_filter("testField", "TEST_VALUE");
        let arr = filter.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let entry = &arr[0];
        assert_eq!(entry["fieldName"], "testField");
        assert_eq!(entry["comparator"], "EQUALS");
        assert_eq!(entry["fieldValue"]["type"], "STRING");
        assert_eq!(entry["fieldValue"]["value"], "TEST_VALUE");
    }
}
