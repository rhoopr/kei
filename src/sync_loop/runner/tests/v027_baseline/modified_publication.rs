//! Fresh prepared output must retain stronger provenance than historical hashes.
use super::{BaselineSession, MEDIA, ZONE, baseline_owner, generation, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone)]
struct CurrentView(BaselineSession);

#[async_trait::async_trait]
impl PhotosSession for CurrentView {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if !url.contains("/records/lookup?") {
            return self.0.post(url, body, headers).await;
        }
        let request: Value = serde_json::from_str(&body)?;
        self.0
            .requests
            .lock()
            .unwrap()
            .push((url.into(), request.clone()));
        // Changes describe observations; lookup returns the authoritative
        // current view once per identity, never both revisions of an asset.
        let mut current = std::collections::BTreeMap::new();
        for record in self.0.inventory.iter().chain(self.0.concurrent.iter()) {
            current.insert(
                record["recordName"].as_str().unwrap().to_owned(),
                record.clone(),
            );
        }
        if self.0.cycle > 0 {
            for record in self.0.incremental_addition.iter() {
                current.insert(
                    record["recordName"].as_str().unwrap().to_owned(),
                    record.clone(),
                );
            }
        }
        let records: Vec<_> = request["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|requested| {
                current
                    .get(requested["recordName"].as_str().unwrap())
                    .cloned()
            })
            .collect();
        Ok(json!({"records": records}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_owned_modified_publication_survives_observation_and_two_quiet_reopens() {
    for database in ["private", "shared"] {
        modified_publication(database).await;
    }
}

async fn modified_publication(database: &'static str) {
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let server = crate::start_wiremock_or_skip!();
    for endpoint in ["/first", "/second", "/concurrent"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let sentinel = root.join("unrelated-existing.bin");
    std::fs::write(&sentinel, b"existing untouched bytes").unwrap();
    let sentinel_mtime = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    let db_path = dir.path().join("state.db");
    let first = pair("first", "first.JPG", &format!("{}/first", server.uri()), 11);
    let mut inventory = first.clone();
    inventory.extend(pair(
        "second",
        "second.JPG",
        &format!("{}/second", server.uri()),
        12,
    ));
    let concurrent = pair(
        "concurrent",
        "concurrent.JPG",
        &format!("{}/concurrent", server.uri()),
        13,
    );
    let mut changed = first;
    changed[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    let mut published = None;
    let source = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(MEDIA))
    };
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = BaselineSession {
            database,
            cycle,
            inventory: Arc::new(inventory.clone()),
            concurrent: Arc::new(concurrent.clone()),
            incremental_addition: Arc::new(changed.clone()),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            format!("https://synthetic.example/database/1/photos/production/{database}"),
            Arc::new(std::collections::HashMap::new()),
            Box::new(CurrentView(session.clone())),
            Arc::new(zone()),
            database.into(),
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
            config.metadata.set_exif_datetime = true;
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
        assert_eq!(
            result.failed_count,
            0,
            "cycle {cycle}: {result:?}; trace={:?}",
            trace.events()
        );
        assert_eq!(
            result.stats.skipped.uncertain, 0,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 3 } else { 0 },
            "{result:?}"
        );
        assert!(result.db_sync_token_advance_safe, "{result:?}");
        if cycle > 0 {
            assert_eq!(session.inventory_queries.load(Ordering::SeqCst), 0);
            assert_eq!(session.boundaries.load(Ordering::SeqCst), 0);
        }
        let output = root.join("first.JPG");
        let bytes = std::fs::read(&output).unwrap();
        assert_ne!(
            bytes, MEDIA,
            "fixture must exercise an actual embedded rewrite"
        );
        let mtime = std::fs::metadata(&output).unwrap().modified().unwrap();
        let local = download::file::compute_sha256(&output).await.unwrap();
        assert_ne!(local, source);
        if let Some((prior_bytes, prior_mtime)) = &published {
            assert_eq!(&bytes, prior_bytes);
            assert_eq!(&mtime, prior_mtime);
        }
        published = Some((bytes, mtime));
        {
            let conn = db
                .acquire_lock("fresh edited publication provenance")
                .unwrap();
            let receipt:(String,String,String,i64,String,String) = conn.query_row(
            "SELECT source_checksum,local_checksum,provider_checksum,provider_size,master,state_id FROM download_receipts WHERE library=?1 AND child='asset-first' AND version_size='original'",
            [ZONE],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
            assert_eq!(
                receipt,
                (
                    source.clone(),
                    local,
                    generation(11),
                    MEDIA.len() as i64,
                    "first".into(),
                    "asset-first".into()
                )
            );
        }
        assert!(db.get_downloaded_page(0, 100).await.unwrap().iter().any(
            |row| row.library.as_ref() == ZONE
                && row.id.as_ref() == "asset-first"
                && row.local_path.as_ref() == Some(&output)
        ));
        assert_eq!(
            db.get_metadata(&key).await.unwrap(),
            Some(format!("v27-after-cycle-{}", cycle + 1))
        );
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"existing untouched bytes"
        );
        assert_eq!(
            std::fs::metadata(&sentinel).unwrap().modified().unwrap(),
            sentinel_mtime
        );
    }
    server.verify().await;
}
