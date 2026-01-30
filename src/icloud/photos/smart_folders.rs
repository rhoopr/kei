use serde_json::{json, Value};

pub struct FolderDef {
    pub obj_type: &'static str,
    pub list_type: &'static str,
    pub query_filter: Option<Value>,
}

pub fn smart_folder_filter(field: &str, value: &str) -> Value {
    json!([{
        "fieldName": field,
        "comparator": "EQUALS",
        "fieldValue": {"type": "STRING", "value": value}
    }])
}

pub fn smart_folders() -> Vec<(&'static str, FolderDef)> {
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
