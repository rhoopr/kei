//! Indexing qualification must not resume obsolete archive layout convergence.
use super::{BaselineSession, MEDIA, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::state::db::primary_layout::{LayoutBinding, LayoutFile, LayoutFingerprint};
use crate::state::db::provider_selection::SelectionPath;
use crate::sync_cycle::{
    indexing_materialization_key, indexing_reconciliation_key, run_cycle, sync_token_key,
};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone)]
struct IndexingView(BaselineSession);

#[async_trait::async_trait]
impl PhotosSession for IndexingView {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState",
                "fields":{"state":{"value":if self.0.cycle==0 {"RUNNING"} else {"FINISHED"}}}}]}),
            );
        }
        if url.contains("/changes/zone?") {
            self.0.changes.fetch_add(1, Ordering::SeqCst);
            self.0
                .requests
                .lock()
                .unwrap()
                .push((url.into(), request.clone()));
            return Ok(json!({"zones":[{"zoneID":zone(),"records":[],
                "syncToken":format!("indexing-current-{}",self.0.cycle),"moreComing":false}]}));
        }
        self.0.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_indexing_running_finished_quiet_preserves_recorded_layout() {
    let server = crate::start_wiremock_or_skip!();
    for endpoint in ["/first", "/second"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let old = root.join("historical-edited.JPG");
    std::fs::write(&old, b"preserved historical metadata-edited bytes").unwrap();
    let bytes = std::fs::read(&old).unwrap();
    let mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
    let binding = LayoutBinding {
        family: "historical-family".into(),
        library: ZONE.into(),
        source_library: ZONE.into(),
        child: "historical-child".into(),
        generation: 1,
        decision: "recorded-decision".into(),
        policy: "recorded-policy".into(),
        qualifier: "recorded-qualifier".into(),
        files: vec![LayoutFile {
            path: SelectionPath::from_path(&old),
            version: "original".into(),
            provider_checksum: "historical-provider-generation".into(),
            fingerprint: LayoutFingerprint {
                size: bytes.len() as u64,
                sha256: Sha256::digest(&bytes).into(),
                identity: crate::fs_util::file_identity(&std::fs::File::open(&old).unwrap())
                    .unwrap(),
            },
            source_checksum: None,
            sidecar: None,
            archive_original: false,
            metadata_decision: None,
        }],
    };
    let binding_bytes = serde_json::to_vec(&binding).unwrap();
    let db_path = dir.path().join("state.db");
    let mut inventory = pair("first", "first.JPG", &format!("{}/first", server.uri()), 11);
    inventory.extend(pair(
        "second",
        "second.JPG",
        &format!("{}/second", server.uri()),
        12,
    ));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 0 {
            db.acquire_lock("seed historical layout")
                .unwrap()
                .execute(
                    "INSERT INTO primary_layout_bindings VALUES(?1,?2,?2,?3,1,?4,?5,?6)",
                    rusqlite::params![
                        binding.family,
                        ZONE,
                        binding.child,
                        binding.decision,
                        binding.policy,
                        binding_bytes
                    ],
                )
                .unwrap();
        }
        let session = BaselineSession {
            database: "private",
            cycle,
            inventory: Arc::new(inventory.clone()),
            concurrent: Arc::new(Vec::new()),
            incremental_addition: Arc::new(Vec::new()),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(IndexingView(session.clone())),
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
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.edited_naming = crate::types::EditedNaming::Primary;
            Arc::new(config)
        };
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
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 2 } else { 0 },
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(std::fs::read(&old).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&old).unwrap().modified().unwrap(), mtime);
        {
            let conn = db.acquire_lock("unchanged historical layout").unwrap();
            let saved: Vec<u8> = conn
                .query_row(
                    "SELECT binding FROM primary_layout_bindings WHERE family=?1",
                    [&binding.family],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(saved, binding_bytes);
            let operations: i64 = conn
                .query_row("SELECT count(*) FROM primary_layout_operations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(operations, 0, "ordinary indexing must not plan convergence");
        }
        assert_eq!(
            db.get_metadata(&indexing_materialization_key(ZONE))
                .await
                .unwrap(),
            None
        );
        if cycle < 2 {
            assert!(
                !result.db_sync_token_advance_safe,
                "unqualified inventory cannot save token"
            );
            assert_eq!(db.get_metadata(&key).await.unwrap(), None);
            assert_eq!(session.changes.load(Ordering::SeqCst), 0);
        } else {
            assert!(
                result.db_sync_token_advance_safe,
                "cycle {cycle}: {result:?}"
            );
            assert_eq!(
                db.get_metadata(&key).await.unwrap(),
                Some(format!("indexing-current-{cycle}"))
            );
            assert_eq!(
                db.get_metadata(&indexing_reconciliation_key(ZONE))
                    .await
                    .unwrap(),
                None
            );
        }
        if cycle == 3 {
            assert_eq!(
                session.inventory_queries.load(Ordering::SeqCst),
                0,
                "quiet cycle must be incremental"
            );
            assert_eq!(session.boundaries.load(Ordering::SeqCst), 0);
        }
    }
}
