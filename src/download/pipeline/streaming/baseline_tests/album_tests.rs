use super::{STILL, asset, config, current_rows, old_rows, run, seed};
use crate::download::pipeline::streaming::{StreamRuntime, stream_and_download_from_stream};
use crate::download::{DownloadControls, DownloadReporting, DownloadRunMode};
use crate::state::SqliteStateDb;
use futures_util::stream;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn baseline_stream_named_album_does_not_claim_other_parent_as_physical_copy() {
    check_album_copy(false).await;
    check_album_copy(true).await;
}

async fn check_album_copy(same_current_identity: bool) {
    let server = crate::start_wiremock_or_skip!();
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/still"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(STILL))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let media = root.join("Old Album/renamed.JPG");
    let db_path = dir.path().join("state.db");
    seed(&db_path, &media).await;
    let before = old_rows(&db_path);
    let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
    let mut own = None;
    if same_current_identity {
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let previous = run(
            &root,
            db,
            vec![asset(&server, false)],
            DownloadRunMode::Download,
        )
        .await;
        assert_eq!(previous.downloaded, 1);
        assert_eq!(previous.skip_summary.on_disk, 0);
        let rows = current_rows(&db_path);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "downloaded");
        let path = std::path::PathBuf::from(&rows[0].2);
        assert_ne!(path, media);
        assert_eq!(std::fs::read(&path).unwrap(), STILL);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        own = Some((path, modified));
    }
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let mut cfg = (*config(&root, db)).clone();
    // Effective config produced by with_pass for the default named-album template.
    cfg.folder_structure = "Trip".into();
    cfg.album_name = Some(Arc::from("Trip"));
    cfg.primary_layout_pass_kind = Some(crate::commands::PassKind::Album);
    let requested = crate::download::filter::expected_paths_for(&asset(&server, false), &cfg)
        .remove(0)
        .path;
    assert!(requested.starts_with(root.join("Trip")));
    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<_, anyhow::Error>(asset(&server, false))]),
        &Arc::new(cfg),
        DownloadControls::new(DownloadRunMode::Download, DownloadReporting::hidden()),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&media).unwrap(), STILL);
    assert_eq!(
        std::fs::metadata(&media).unwrap().modified().unwrap(),
        mtime
    );
    assert_eq!(old_rows(&db_path), before);
    assert_eq!(
        result.downloaded,
        usize::from(!same_current_identity),
        "only foreign-only proof requires a new Trip download"
    );
    assert_eq!(requested.exists(), !same_current_identity);
    if let Some((path, modified)) = own {
        assert_eq!(std::fs::read(&path).unwrap(), STILL);
        let after = std::fs::metadata(path).unwrap().modified().unwrap();
        assert_eq!(after, modified);
    }
    let rows = current_rows(&db_path);
    if !requested.exists() {
        assert!(
            !current_rows(&db_path).is_empty(),
            "unfulfilled pass must retain durable work"
        );
        assert_eq!(
            result.skip_summary.on_disk, 0,
            "another album's receipt does not fulfill this physical pass"
        );
        assert!(
            current_rows(&db_path)
                .iter()
                .all(|(_, status, _)| status == "pending"),
            "unfulfilled album copy remains pending"
        );
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let pending = db.pending_match_status(1).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempt.child, "current-child");
        assert_eq!(pending[0].attempt.destination.to_path(), requested);
    } else {
        assert_eq!(std::fs::read(&requested).unwrap(), STILL);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "downloaded");
        assert_eq!(rows[0].2, requested.to_str().unwrap());
    }
    eprintln!("current={same_current_identity} rows={rows:?}");
    server.verify().await;
}
