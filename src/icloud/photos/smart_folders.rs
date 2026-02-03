//! Apple's built-in smart albums (Favorites, Videos, etc.) — these exist
//! for every iCloud account and use server-side query filters rather than
//! explicit membership lists.

use serde_json::{json, Value};

#[derive(Debug)]
pub(crate) struct FolderDef {
    pub(crate) obj_type: &'static str,
    pub(crate) list_type: &'static str,
    pub(crate) query_filter: Option<Value>,
}

pub(crate) fn smart_folder_filter(field: &str, value: &str) -> Value {
    json!([{
        "fieldName": field,
        "comparator": "EQUALS",
        "fieldValue": {"type": "STRING", "value": value}
    }])
}

pub(crate) fn smart_folders() -> Vec<(&'static str, FolderDef)> {
    vec![
        (
            "Time-lapse",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Timelapse",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "TIMELAPSE")),
            },
        ),
        (
            "Videos",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Video",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "VIDEO")),
            },
        ),
        (
            "Slo-mo",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Slomo",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "SLOMO")),
            },
        ),
        (
            "Bursts",
            FolderDef {
                obj_type: "CPLAssetBurstStackAssetByAssetDate",
                list_type: "CPLBurstStackAssetAndMasterByAssetDate",
                query_filter: None,
            },
        ),
        (
            "Favorites",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Favorite",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "FAVORITE")),
            },
        ),
        (
            "Panoramas",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Panorama",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "PANORAMA")),
            },
        ),
        (
            "Screenshots",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Screenshot",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "SCREENSHOT")),
            },
        ),
        (
            "Live",
            FolderDef {
                obj_type: "CPLAssetInSmartAlbumByAssetDate:Live",
                list_type: "CPLAssetAndMasterInSmartAlbumByAssetDate",
                query_filter: Some(smart_folder_filter("smartAlbum", "LIVE")),
            },
        ),
        (
            "Recently Deleted",
            FolderDef {
                obj_type: "CPLAssetDeletedByExpungedDate",
                list_type: "CPLAssetAndMasterDeletedByExpungedDate",
                query_filter: None,
            },
        ),
        (
            "Hidden",
            FolderDef {
                obj_type: "CPLAssetHiddenByAssetDate",
                list_type: "CPLAssetAndMasterHiddenByAssetDate",
                query_filter: None,
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
}
