use super::{PhotoAlbum, QUERY_ALL_LIST, QUERY_FOLDER_LIST};
use serde_json::json;
use std::sync::Arc;

#[test]
fn token_equivalence_provider_categories_never_return_provider_values() {
    let reference = PhotoAlbum::stub_for_test(Arc::from("PRIVATE_ALBUM"));
    let mut album = reference.clone();
    for (query, expected) in [
        (QUERY_ALL_LIST, "library_wide"),
        (QUERY_FOLDER_LIST, "album_membership"),
        ("CPLAssetAndMasterInSmartAlbumByAssetDate", "smart_album"),
        ("CPLBurstStackAssetAndMasterByAssetDate", "burst"),
        ("CPLAssetAndMasterDeletedByExpungedDate", "deleted"),
        ("CPLAssetAndMasterHiddenByAssetDate", "hidden"),
        ("PRIVATE_QUERY", "other"),
    ] {
        album.list_type = Arc::from(query);
        assert_eq!(
            album.token_diagnostic_categories(&reference, "PrimarySync"),
            (expected, "same_first_pass")
        );
    }
    for zone in [
        json!({"zoneName":"PrimarySync","ownerRecordName":"PRIVATE_OWNER"}),
        json!({"zoneName":"PrimarySync","ownerRecordName":12345}),
    ] {
        album.zone_id = Arc::new(zone);
        assert_eq!(
            album
                .token_diagnostic_categories(&reference, "PrimarySync")
                .1,
            "different_first_pass"
        );
    }
    album.zone_id = Arc::new(json!({"zoneName":"PRIVATE_ZONE"}));
    assert_eq!(
        album
            .token_diagnostic_categories(&reference, "PrimarySync")
            .1,
        "different_selected_zone"
    );
    album.zone_id = Arc::new(json!({}));
    assert_eq!(
        album
            .token_diagnostic_categories(&reference, "PrimarySync")
            .1,
        "unavailable"
    );
    album = reference.clone();
    album.service_endpoint = Arc::from("PRIVATE_ENDPOINT");
    assert_eq!(
        album
            .token_diagnostic_categories(&reference, "PrimarySync")
            .1,
        "different_first_pass"
    );
    album = reference.clone();
    album.container_id = Some(Arc::from("PRIVATE_CONTAINER"));
    assert_eq!(
        album
            .token_diagnostic_categories(&reference, "PrimarySync")
            .1,
        "different_first_pass"
    );
}
