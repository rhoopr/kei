use std::sync::Arc;

use serde_json::json;

use crate::commands::PassKind;
use crate::download::DownloadOutcome;
use crate::types::{FileMatchPolicy, LivePhotoMovFilenamePolicy, RawPolicy};

use super::support::Harness;
use super::{cycle_passes, pass};

#[tokio::test]
async fn bundled_filename_and_folder_policies() {
    for (unicode, policy, expected) in [
        (
            true,
            FileMatchPolicy::NameSizeDedupWithSuffix,
            "Café_🧠.JPG",
        ),
        (false, FileMatchPolicy::NameSizeDedupWithSuffix, "Caf_.JPG"),
        (true, FileMatchPolicy::NameId7, "Café_🧠_YXNzZXQ.JPG"),
    ] {
        let mut h = Harness::new().await;
        h.config.keep_unicode_in_filenames = unicode;
        h.config.file_match_policy = policy;
        let records = h
            .asset(
                "fixture-master",
                "Café_🧠.jpg",
                "public.jpeg",
                "media/pattern.jpg",
                1,
            )
            .await;
        h.stable(records, &[(expected, "media/pattern.jpg")]).await;
    }
    for album in [false, true] {
        let mut h = Harness::new().await;
        h.config.folder_structure = "%Y".into();
        h.config.folder_structure_albums = Arc::from("{album}/%Y");
        let records = h
            .asset("folder", "Photo.jpg", "public.jpeg", "media/pattern.jpg", 1)
            .await;
        let expected = if album {
            "Controlled/2023/Photo.JPG"
        } else {
            "2023/Photo.JPG"
        };
        for round in 0..2 {
            let mut pass = pass(records.clone());
            if album {
                pass.kind = PassKind::Album;
                pass.album.name = Arc::from("Controlled");
            }
            let result = cycle_passes(&h.config, &[pass]).await;
            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "{result:?}"
            );
            assert_eq!(result.stats.downloaded, usize::from(round == 0));
            h.assert_files(&[(expected, "media/pattern.jpg")]).await;
            h.reopen().await;
        }
        h.server.verify().await;
    }
}

#[tokio::test]
async fn bundled_raw_and_companion_naming() {
    for policy in [RawPolicy::AsIs, RawPolicy::PreferRaw, RawPolicy::PreferJpeg] {
        let mut h = Harness::new().await;
        h.config.raw_policy = policy;
        h.config.alternative = true;
        let mut records = h
            .asset(
                "raw-pair",
                "Pair.jpg",
                "public.jpeg",
                "media/pattern.jpg",
                1,
            )
            .await;
        records[0]["fields"]["resOriginalAltRes"] = h.resource("raw", "media/pattern.dng", 1).await;
        records[0]["fields"]["resOriginalAltFileType"] = json!({"value": "com.adobe.raw-image"});
        let expected = if policy == RawPolicy::PreferRaw {
            vec![
                ("Pair.DNG", "media/pattern.dng"),
                ("Pair_alt.JPG", "media/pattern.jpg"),
            ]
        } else {
            vec![
                ("Pair.JPG", "media/pattern.jpg"),
                ("Pair_RAW.DNG", "media/pattern.dng"),
            ]
        };
        h.stable(records, &expected).await;
    }
    for policy in [
        LivePhotoMovFilenamePolicy::Suffix,
        LivePhotoMovFilenamePolicy::Original,
    ] {
        let mut h = Harness::new().await;
        h.config.live_photo_mov_filename_policy = policy;
        let mut records = h
            .asset(
                "live",
                "Live.HEIC",
                "public.heic",
                "media/apple-live.heic",
                1,
            )
            .await;
        records[0]["fields"]["resOriginalVidComplRes"] =
            h.resource("motion", "media/apple-live.mov", 1).await;
        records[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value": "com.apple.quicktime-movie"});
        let movie = if policy == LivePhotoMovFilenamePolicy::Suffix {
            "Live_HEVC.MOV"
        } else {
            "Live.MOV"
        };
        h.stable(
            records,
            &[
                ("Live.HEIC", "media/apple-live.heic"),
                (movie, "media/apple-live.mov"),
            ],
        )
        .await;
    }
}
