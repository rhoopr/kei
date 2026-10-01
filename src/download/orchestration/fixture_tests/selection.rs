use serde_json::json;

use crate::config::{MediaSelection, parse_created_date_filter};
use crate::types::{LivePhotoMode, PhotoResolution};

use super::support::Harness;

#[tokio::test]
async fn bundled_media_filters_select_exact_assets() {
    for (selection, mode, photo, video, live) in [
        (
            MediaSelection {
                photos: true,
                videos: false,
                live_photos: false,
            },
            LivePhotoMode::Both,
            true,
            false,
            false,
        ),
        (
            MediaSelection {
                photos: false,
                videos: true,
                live_photos: false,
            },
            LivePhotoMode::Both,
            false,
            true,
            false,
        ),
        (
            MediaSelection {
                photos: false,
                videos: false,
                live_photos: true,
            },
            LivePhotoMode::Both,
            false,
            false,
            true,
        ),
        (
            MediaSelection::all(),
            LivePhotoMode::Skip,
            true,
            true,
            false,
        ),
        (MediaSelection::all(), LivePhotoMode::Both, true, true, true),
    ] {
        let mut h = Harness::new().await;
        h.config.media = selection;
        h.config.live_photo_mode = mode;
        let mut records = h
            .asset(
                "photo",
                "Still.jpg",
                "public.jpeg",
                "media/pattern.jpg",
                u64::from(photo),
            )
            .await;
        records.extend(
            h.asset(
                "video",
                "Video.MOV",
                "com.apple.quicktime-movie",
                "media/pattern.mov",
                u64::from(video),
            )
            .await,
        );
        let mut pair = h
            .asset(
                "live",
                "Live.HEIC",
                "public.heic",
                "media/apple-live.heic",
                u64::from(live),
            )
            .await;
        pair[0]["fields"]["resOriginalVidComplRes"] = h
            .resource("motion", "media/apple-live.mov", u64::from(live))
            .await;
        pair[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value": "com.apple.quicktime-movie"});
        records.extend(pair);
        let mut expected = Vec::new();
        if photo {
            expected.push(("Still.JPG", "media/pattern.jpg"));
        }
        if video {
            expected.push(("Video.MOV", "media/pattern.mov"));
        }
        if live {
            expected.extend([
                ("Live.HEIC", "media/apple-live.heic"),
                ("Live_HEVC.MOV", "media/apple-live.mov"),
            ]);
        }
        h.stable(records, &expected).await;
    }
}

#[tokio::test]
async fn bundled_date_filters_select_exact_assets() {
    for (before, after, selected) in [
        (Some("2020-01-01"), None, "new"),
        (None, Some("2020-01-01"), "old"),
    ] {
        let mut h = Harness::new().await;
        h.config.skip_created_before =
            before.map(|value| parse_created_date_filter(value).unwrap());
        h.config.skip_created_after = after.map(|value| parse_created_date_filter(value).unwrap());
        let mut records = Vec::new();
        for (id, date) in [("new", 1_672_531_200_000_i64), ("old", 1_546_300_800_000)] {
            let mut pair = h
                .asset(
                    id,
                    &format!("{id}.jpg"),
                    "public.jpeg",
                    "media/pattern.jpg",
                    u64::from(id == selected),
                )
                .await;
            pair[1]["fields"]["assetDate"] = json!({"value": date, "type": "TIMESTAMP"});
            records.extend(pair);
        }
        h.stable(
            records,
            &[(format!("{selected}.JPG").as_str(), "media/pattern.jpg")],
        )
        .await;
    }
}

#[tokio::test]
async fn bundled_rendition_selection_and_fallback() {
    for (available, forced) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut h = Harness::new().await;
        h.config.resolution = PhotoResolution::Medium;
        h.config.force_resolution = forced;
        let mut pair = h
            .asset(
                "rendition",
                "Sized.jpg",
                "public.jpeg",
                "media/metadata.jpg",
                u64::from(!available && !forced),
            )
            .await;
        if available {
            pair[0]["fields"]["resJPEGMedRes"] = h.resource("medium", "media/pattern.jpg", 1).await;
            pair[0]["fields"]["resJPEGMedFileType"] = json!({"value": "public.jpeg"});
            pair[0]["fields"]["resJPEGMedWidth"] = json!({"value": 48});
            pair[0]["fields"]["resJPEGMedHeight"] = json!({"value": 32});
        }
        let expected = if available {
            vec![("Sized-medium.JPG", "media/pattern.jpg")]
        } else if forced {
            vec![]
        } else {
            vec![("Sized.JPG", "media/metadata.jpg")]
        };
        h.stable(pair, &expected).await;
    }
    let mut h = Harness::new().await;
    h.config.resolution = PhotoResolution::Medium;
    let raw = h
        .asset(
            "raw",
            "Camera.DNG",
            "com.adobe.raw-image",
            "media/pattern.dng",
            1,
        )
        .await;
    h.stable(raw, &[("Camera.DNG", "media/pattern.dng")]).await;
}
