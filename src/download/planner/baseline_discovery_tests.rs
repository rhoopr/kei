use super::TaskPlanner;
use crate::download::DownloadConfig;
use crate::download::baseline_matching::BaselineMatch;
use crate::test_helpers::TestPhotoAsset;
use sha1::{Digest, Sha1};

fn signature(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    data_encoding::BASE64.encode(&[vec![1], hash.finalize().to_vec()].concat())
}

fn fixture(root: &std::path::Path) -> (DownloadConfig, crate::icloud::photos::PhotoAsset) {
    let mut config = DownloadConfig::test_default();
    config.directory = root.to_path_buf().into();
    config.folder_structure = "none".into();
    config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
    let asset = TestPhotoAsset::new("current-child-identity")
        .filename("IMG_0001.JPG")
        .orig_file_type("public.jpeg")
        .orig_size(4)
        .orig_checksum(&signature(b"good"))
        .build();
    (config, asset)
}

#[tokio::test]
async fn v027_discovery_reuses_old_unsuffixed_and_old_identity_family_without_receipts() {
    for name in [
        "IMG_0001.JPG",
        "IMG_0001-old-primary-identity.JPG",
        "IMG_0001_4.JPG",
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(name);
        std::fs::write(&path, b"good").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let (config, asset) = fixture(root.path());
        let mut planner = TaskPlanner::new();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        let task = plan.tasks.first().unwrap();
        let BaselineMatch::ProviderPresent { receipt, file } =
            planner.match_existing(&asset, &config, task).await
        else {
            panic!("same-directory historical raw file should be reused: {name}");
        };
        assert_eq!(receipt.local_path, path);
        assert_eq!(receipt.id, task.asset_id.as_ref());
        file.validate().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"good");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
    }
}

#[tokio::test]
async fn v027_discovery_wrong_size_edited_or_same_size_wrong_bytes_stay_uncertain() {
    for bytes in [b"metadata edited".as_slice(), b"evil".as_slice()] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("IMG_0001-old-id.JPG");
        std::fs::write(&path, bytes).unwrap();
        let (config, asset) = fixture(root.path());
        let mut planner = TaskPlanner::new();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert!(matches!(
            planner
                .match_existing(&asset, &config, plan.tasks.first().unwrap())
                .await,
            BaselineMatch::Uncertain
        ));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn v027_discovery_missing_directory_is_complete_but_failed_directory_is_not() {
    let root = tempfile::tempdir().unwrap();
    let (config, asset) = fixture(root.path());
    let mut planner = TaskPlanner::new();
    let missing = planner
        .discover_baseline_family(&asset, &root.path().join("missing/IMG_0001.JPG"))
        .await;
    assert!(missing.complete);
    assert!(missing.paths.is_empty());
    std::fs::write(root.path().join("blocked"), b"not a directory").unwrap();
    let failed = planner
        .discover_baseline_family(&asset, &root.path().join("blocked/IMG_0001.JPG"))
        .await;
    assert!(!failed.complete);
    let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    let mut blocked_config = config.clone();
    blocked_config.folder_structure = "blocked".into();
    assert!(matches!(
        planner
            .match_existing(&asset, &blocked_config, plan.tasks.first().unwrap())
            .await,
        BaselineMatch::Uncertain
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn v027_discovery_symlink_family_does_not_adopt_or_disappear_as_missing() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("unrelated.JPG");
    std::fs::write(&target, b"good").unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("IMG_0001-old-id.JPG")).unwrap();
    let (config, asset) = fixture(root.path());
    let mut planner = TaskPlanner::new();
    let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    assert!(matches!(
        planner
            .match_existing(&asset, &config, plan.tasks.first().unwrap())
            .await,
        BaselineMatch::Uncertain
    ));
    assert_eq!(std::fs::read(target).unwrap(), b"good");
}

#[tokio::test]
async fn v027_discovery_stays_in_requested_parent_and_does_not_confuse_prefix_siblings() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("other-album")).unwrap();
    std::fs::write(root.path().join("other-album/IMG_0001.JPG"), b"good").unwrap();
    std::fs::write(root.path().join("IMG_00010.JPG"), b"good").unwrap();
    std::fs::write(root.path().join("arbitrary-rename.JPG"), b"good").unwrap();
    let (config, asset) = fixture(root.path());
    let mut planner = TaskPlanner::new();
    let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    assert!(matches!(
        planner
            .match_existing(&asset, &config, plan.tasks.first().unwrap())
            .await,
        BaselineMatch::Missing
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn v027_discovery_partial_listing_keeps_visible_candidates_and_defers_unknown_formats() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("IMG_0001-old-id.JPG");
    std::fs::write(&path, b"good").unwrap();
    std::fs::write(
        root.path().join(std::ffi::OsString::from_vec(vec![0xff])),
        b"unindexed",
    )
    .unwrap();
    let (config, _) = fixture(root.path());
    let asset = TestPhotoAsset::new("current-child-identity")
        .filename("IMG_0001.JPG")
        .orig_file_type("public.jpeg")
        .orig_size(4)
        .orig_checksum("unsupported-provider-format")
        .build();
    let mut planner = TaskPlanner::new();
    let discovered = planner
        .discover_baseline_family(&asset, &root.path().join("IMG_0001.JPG"))
        .await;
    assert!(!discovered.complete);
    assert!(discovered.paths.contains(&path));
    let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    assert!(matches!(
        planner
            .match_existing(&asset, &config, plan.tasks.first().unwrap())
            .await,
        BaselineMatch::Uncertain
    ));
}
