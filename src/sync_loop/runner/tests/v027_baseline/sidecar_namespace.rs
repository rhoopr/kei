//! Current selected metadata retries must not follow a replaced parent.
use super::{BaselineSession, MEDIA, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::PhotoLibrary;
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, ResponseTemplate, matchers::method};

#[tokio::test]
async fn current_sidecar_quiet_retry_refuses_replaced_parent_then_recovers() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(3)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    let parent = root.join("album");
    let retained = root.join("retained");
    std::fs::create_dir_all(parent.join("missing.JPG.xmp")).unwrap();
    let db_path = dir.path().join("state.db");
    let mut records = pair(
        "existing",
        "existing.JPG",
        &format!("{}/existing", server.uri()),
        1,
    );
    records.extend(pair(
        "missing",
        "missing.JPG",
        &format!("{}/missing", server.uri()),
        2,
    ));
    let concurrent = pair(
        "concurrent",
        "concurrent.JPG",
        &format!("{}/concurrent", server.uri()),
        3,
    );
    let (_, shared) = make_shared_session_for_run_cycle().await;
    let external_media = outside.path().join("missing.JPG");
    let external_sidecar = outside.path().join("missing.JPG.xmp");
    let packet = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='4'/></rdf:RDF></x:xmpmeta>";
    std::fs::write(&external_media, MEDIA).unwrap();
    std::fs::write(&external_sidecar, packet).unwrap();
    let parsed: xmp_toolkit::XmpMeta = std::str::from_utf8(packet).unwrap().parse().unwrap();
    assert_eq!(
        parsed
            .property(xmp_toolkit::xmp_ns::XMP, "Rating")
            .unwrap()
            .value,
        "4"
    );
    let stamp = std::fs::metadata(&external_sidecar)
        .unwrap()
        .modified()
        .unwrap();
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 1 {
            std::fs::rename(&parent, &retained).unwrap();
            std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
        }
        if cycle == 2 {
            std::fs::remove_file(&parent).unwrap();
            std::fs::rename(&retained, &parent).unwrap();
            std::fs::remove_dir(parent.join("missing.JPG.xmp")).unwrap();
        }
        if cycle > 0 {
            db.make_selection_retry_due(baseline_owner(), "asset-missing".into())
                .await
                .unwrap();
        }
        let session = BaselineSession {
            database: "private",
            cycle,
            inventory: Arc::new(records.clone()),
            concurrent: Arc::new(concurrent.clone()),
            incremental_addition: Arc::new(Vec::new()),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session.clone()),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        }];
        let mut selected =
            make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, library| {
            let mut config = (*base(mode, exclude, groups, library)).clone();
            config.folder_structure = "album".into();
            config.metadata.xmp_sidecar = true;
            Arc::new(config)
        };
        let before_attempts: i64 = db.acquire_lock("metadata retry attempt before cycle").unwrap()
            .query_row("SELECT COALESCE(SUM(attempts),0) FROM provider_active_decisions WHERE child='asset-missing'", [], |row| row.get(0)).unwrap();
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 3 } else { 0 },
            "{result:?}"
        );
        if cycle > 0 {
            assert_eq!(session.inventory_queries.load(Ordering::SeqCst), 0);
        }
        assert_eq!(
            std::fs::read(&external_sidecar).unwrap(),
            packet,
            "cycle{cycle}: outside sidecar must not change"
        );
        assert_eq!(
            std::fs::metadata(&external_sidecar)
                .unwrap()
                .modified()
                .unwrap(),
            stamp
        );
        assert_eq!(std::fs::read(&external_media).unwrap(), MEDIA);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 2);
        let status = db.observed_selection_status().await.unwrap();
        assert_eq!(status.len(), 1);
        if cycle == 1 {
            let after_attempts: i64 = db.acquire_lock("metadata retry attempt after refusal").unwrap()
                .query_row("SELECT COALESCE(SUM(attempts),0) FROM provider_active_decisions WHERE child='asset-missing'", [], |row| row.get(0)).unwrap();
            assert!(
                after_attempts > before_attempts,
                "due retry must actually run: {result:?}"
            );
        }

        assert_eq!(
            status[0].pending_metadata,
            cycle < 2,
            "cycle{cycle}: {result:?}"
        );
        if cycle == 2 {
            assert!(parent.join("missing.JPG.xmp").is_file());
        }
    }
    server.verify().await;
}
