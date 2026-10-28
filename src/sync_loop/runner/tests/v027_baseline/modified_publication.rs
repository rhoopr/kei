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

// A changed rendition is still one selected asset. The baseline harness's
// addition-oriented count would otherwise count this replacement twice.
#[derive(Clone)]
struct ReplacementView(CurrentView);

#[async_trait::async_trait]
impl PhotosSession for ReplacementView {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let result = self.0.post(url, body, headers).await?;
        if self.0.0.cycle > 0 && url.contains("/internal/records/query/batch") {
            return Ok(crate::sync_loop::test_support::album_count_response(3));
        }
        Ok(result)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_recent_same_child_master_changed_resource_preserves_old_bytes() {
    same_child_master_changed_resource(false).await;
}

#[tokio::test]
async fn v027_recent_same_child_master_mmcs_v1_resource_preserves_old_bytes() {
    same_child_master_changed_resource(true).await;
}

async fn same_child_master_changed_resource(mmcs_v1: bool) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let server = crate::start_wiremock_or_skip!();
    let mut changed_bytes = MEDIA.to_vec();
    changed_bytes.push(0);
    let checksum = |bytes: &[u8]| {
        if mmcs_v1 {
            let mut digest = sha1::Sha1::new();
            digest.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
            digest.update(bytes);
            base64::engine::general_purpose::STANDARD
                .encode([vec![1], digest.finalize().to_vec()].concat())
        } else {
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
        }
    };
    let old_checksum = checksum(MEDIA);
    let changed_checksum = checksum(&changed_bytes);
    for (endpoint, bytes) in [
        ("/identity-old", MEDIA.to_vec()),
        ("/identity-new", changed_bytes.clone()),
        ("/identity-second", MEDIA.to_vec()),
        ("/identity-concurrent", MEDIA.to_vec()),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut old = pair(
        "first",
        "first.JPG",
        &format!("{}/identity-old", server.uri()),
        11,
    );
    old[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(old_checksum);
    let mut changed = old.clone();
    changed[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/identity-new", server.uri()));
    changed[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(changed_bytes.len());
    changed[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(changed_checksum);
    assert_eq!(old[0]["recordName"], changed[0]["recordName"]);
    assert_eq!(old[1]["recordName"], changed[1]["recordName"]);
    assert_eq!(
        old[1]["fields"]["masterRef"],
        changed[1]["fields"]["masterRef"]
    );
    let second = pair(
        "second",
        "second.JPG",
        &format!("{}/identity-second", server.uri()),
        12,
    );
    let concurrent = pair(
        "concurrent",
        "concurrent.JPG",
        &format!("{}/identity-concurrent", server.uri()),
        13,
    );
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let key = sync_token_key(ZONE);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut previous_path = None;
    let mut previous_mtime = None;
    let mut current_path = None;

    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let mut inventory = if cycle == 0 {
            old.clone()
        } else {
            changed.clone()
        };
        inventory.extend(second.clone());
        let session = BaselineSession {
            database: "private",
            cycle,
            inventory: Arc::new(inventory),
            concurrent: Arc::new(concurrent.clone()),
            incremental_addition: Arc::new(if cycle == 1 {
                changed.clone()
            } else {
                Vec::new()
            }),
            requests: Arc::new(Mutex::new(Vec::new())),
            inventory_queries: Arc::new(AtomicUsize::new(0)),
            inventory_starts: Arc::new(AtomicUsize::new(0)),
            boundaries: Arc::new(AtomicUsize::new(0)),
            changes: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(ReplacementView(CurrentView(session.clone()))),
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
            config.recent = Some(10);
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
        if cycle > 0 {
            let prior = previous_path.as_ref().unwrap();
            assert_eq!(std::fs::read(prior).unwrap(), MEDIA);
            assert_eq!(
                std::fs::metadata(prior).unwrap().modified().unwrap(),
                previous_mtime.unwrap()
            );
            if result.stats.skipped.uncertain > 0 {
                let pending: i64 = db.acquire_lock("changed current resource remains outstanding").unwrap().query_row(
                    "SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND child='asset-first' AND checksum=?2 AND verified_media=0",
                    rusqlite::params![ZONE,changed_checksum], |row| row.get(0)).unwrap();
                assert!(pending > 0, "changed selected resource must remain durable");
                let reason: Option<String> = db.acquire_lock("synthetic changed resource diagnostic").unwrap().query_row("SELECT last_error FROM assets WHERE library=?1 AND id='asset-first' AND version_size='original'", [ZONE], |row| row.get(0)).unwrap();
                eprintln!("synthetic mmcs_v1={mmcs_v1} match diagnostic: {reason:?}");
            }
        }
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert_eq!(
            result.stats.skipped.uncertain, 0,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.state_write_failures, 0,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded,
            match cycle {
                0 => 3,
                1 => 1,
                _ => 0,
            },
            "cycle {cycle}: {result:?}"
        );
        assert!(
            result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(&key).await.unwrap(),
            Some(format!("v27-after-cycle-{}", cycle + 1))
        );
        let rows = db.get_downloaded_page(0, 100).await.unwrap();
        let row = rows
            .iter()
            .find(|row| row.library.as_ref() == ZONE && row.id.as_ref() == "asset-first")
            .expect("current child row");
        let path = row.local_path.clone().expect("current child path");
        if cycle == 0 {
            assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
            previous_mtime = Some(std::fs::metadata(&path).unwrap().modified().unwrap());
            previous_path = Some(path);
        } else {
            let old_path = previous_path.as_ref().unwrap();
            assert_ne!(&path, old_path);
            assert_eq!(std::fs::read(old_path).unwrap(), MEDIA);
            assert_eq!(std::fs::read(&path).unwrap(), changed_bytes);
            if let Some(prior) = &current_path {
                assert_eq!(&path, prior);
            }
            current_path = Some(path.clone());
            if cycle == 1 {
                let conn = db
                    .acquire_lock("changed resource admitted by authenticated selection")
                    .unwrap();
                let count: i64 = conn.query_row(
                "SELECT count(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library=?1 AND d.child='asset-first' AND d.master='first' AND d.checksum=?2 AND d.size_bytes=?3 AND d.admitted=1 AND d.verified_media=1 AND json_extract(CAST(g.specification AS TEXT),'$.format')=2",
                rusqlite::params![ZONE, changed_checksum, changed_bytes.len() as i64], |r| r.get(0),
            ).unwrap();
                assert!(
                    count > 0,
                    "changed destination must be admitted and verified in format 2"
                );
            }
        }
    }
    server.verify().await;
}
