//! Synthetic large-inventory legacy reuse. Explicit measurement only.
use super::{MEDIA, OLD_TOKEN, ZONE, baseline_owner, historical_rows, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::{DownloadStateStore, SqliteStateDb};
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use base64::Engine as _;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn provider_signature(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    let mut value = vec![1];
    value.extend_from_slice(&hash.finalize());
    base64::engine::general_purpose::STANDARD.encode(value)
}
fn media(index: usize) -> Vec<u8> {
    let mut bytes = MEDIA.to_vec();
    bytes.extend_from_slice(format!("-synthetic-{index:08}").as_bytes());
    bytes
}
fn peak_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}
fn db_bytes(path: &std::path::Path) -> u64 {
    [
        path.to_path_buf(),
        path.with_extension("db-wal"),
        path.with_extension("db-shm"),
    ]
    .iter()
    .map(|p| std::fs::metadata(p).map_or(0, |m| m.len()))
    .sum()
}
#[derive(Clone)]
struct ScaleSession {
    inventory: Arc<Vec<Vec<Value>>>,
    lookup: Arc<HashMap<String, Value>>,
    cycle: usize,
    ranks: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for ScaleSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"syncToken":"scale-boundary","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(self.inventory.len() as u64));
        }
        if url.contains("/records/query?") {
            assert_eq!(request["zoneID"], zone());
            self.ranks.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap() as usize;
            let page = request["resultsLimit"].as_u64().unwrap() as usize / 2;
            let records: Vec<_> = self
                .inventory
                .iter()
                .skip(offset)
                .take(page)
                .flatten()
                .cloned()
                .collect();
            return Ok(json!({"records":records,"syncToken":"scale-inventory-token"}));
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone());
            let expected = if self.cycle == 0 {
                "scale-boundary".to_owned()
            } else {
                format!("scale-after-{}", self.cycle)
            };
            assert_eq!(request["zones"][0]["syncToken"], expected);
            return Ok(
                json!({"zones":[{"zoneID":zone(),"records":[],"syncToken":format!("scale-after-{}",self.cycle+1),"moreComing":false}]}),
            );
        }
        if url.contains("/records/lookup?") {
            let found: Vec<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|wanted| self.lookup.get(wanted["recordName"].as_str()?).cloned())
                .collect();
            return Ok(json!({"records":found}));
        }
        anyhow::bail!("unexpected synthetic endpoint: {url}")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
#[ignore = "synthetic inventory scale measurement; run alone with explicit size"]
async fn issue853_scoped_legacy_reuse_scale() {
    let count: usize = std::env::var("KEI_SCALE_ITEMS")
        .unwrap_or_else(|_| "128".into())
        .parse()
        .unwrap();
    assert!((1..=41_318).contains(&count));
    let total = Instant::now();
    let server = crate::test_helpers::start_wiremock_or_skip("issue853 scale acceptance")
        .await
        .expect("scale fixture requires loopback binding");
    for (endpoint, expected) in [("/qualified", 0), ("/ambiguous", 0), ("/new", 1)] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(expected)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let setup = Instant::now();
    let mut entries = Vec::with_capacity(count);
    let mut inventory = Vec::with_capacity(count + 2);
    {
        let db = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        for index in 0..count {
            let id = format!("q{index:06}");
            let name = format!("IMG_{index:06}.JPG");
            let old_id = format!("legacy-{id}");
            let bytes = media(index);
            let provider = provider_signature(&bytes);
            let file = root.join(&name);
            std::fs::write(&file, &bytes).unwrap();
            let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
            let record = crate::test_helpers::TestAssetRecord::new(&old_id)
                .library("PrimarySync")
                .filename(&name)
                .checksum(&provider)
                .size(bytes.len() as u64)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let local = download::file::compute_sha256(&file).await.unwrap();
            db.mark_verified_download(
                "PrimarySync",
                &old_id,
                "original",
                &file,
                &local,
                None,
                false,
            )
            .await
            .unwrap();
            let mut current = pair(&id, &name, &format!("{}/qualified", server.uri()), 1);
            current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(provider);
            current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(bytes.len());
            inventory.push(current);
            entries.push((id, old_id, file, bytes, mtime, provider, local));
        }
        let wanted = media(count);
        let mut changed = wanted.clone();
        *changed.last_mut().unwrap() ^= 1;
        let ambiguous = root.join("ambiguous.JPG");
        std::fs::write(&ambiguous, &changed).unwrap();
        let ambiguous_mtime = std::fs::metadata(&ambiguous).unwrap().modified().unwrap();
        let provider = provider_signature(&wanted);
        let record = crate::test_helpers::TestAssetRecord::new("legacy-ambiguous")
            .library("PrimarySync")
            .filename("ambiguous.JPG")
            .checksum(&provider)
            .size(wanted.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let local = download::file::compute_sha256(&ambiguous).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "legacy-ambiguous",
            "original",
            &ambiguous,
            &local,
            None,
            false,
        )
        .await
        .unwrap();
        for child in ["history-a", "history-b"] {
            db.upsert_asset_master_mapping("PrimarySync", child, "legacy-ambiguous")
                .await
                .unwrap();
        }
        db.set_metadata_capture_revision_for_test("PrimarySync", "legacy-ambiguous", 0);
        // Find the exact ambiguity candidate; other historical rows may also require metadata.
        let candidate = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.asset_id == "legacy-ambiguous")
            .unwrap();
        assert!(
            db.defer_metadata_capture_ambiguity(&candidate, 1)
                .await
                .unwrap()
        );
        let mut current = pair(
            "ambiguous",
            "ambiguous.JPG",
            &format!("{}/ambiguous", server.uri()),
            2,
        );
        current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(provider);
        current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(wanted.len());
        inventory.push(current);
        let current = pair("new", "new.JPG", &format!("{}/new", server.uri()), 3);
        inventory.push(current);
        db.set_metadata(&format!("sync_token:{ZONE}"), OLD_TOKEN)
            .await
            .unwrap();
        entries.push((
            "ambiguous".into(),
            "legacy-ambiguous".into(),
            ambiguous,
            changed,
            ambiguous_mtime,
            provider,
            local,
        ));
    }
    let original_history = historical_rows(&db_path);
    let mut lookup = HashMap::new();
    for record in inventory.iter().flatten() {
        lookup.insert(
            record["recordName"].as_str().unwrap().to_owned(),
            record.clone(),
        );
    }
    let inventory = Arc::new(inventory);
    let lookup = Arc::new(lookup);
    eprintln!(
        "SCALE_SETUP_REACHED count={count} files={} setup_ms={} db_bytes={} peak_kib={:?}",
        entries.len(),
        setup.elapsed().as_millis(),
        db_bytes(&db_path),
        peak_kib()
    );
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let token_key = sync_token_key(ZONE);
    let mut receipt_snapshot = None;
    for cycle in 0..3 {
        let start = Instant::now();
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = ScaleSession {
            inventory: inventory.clone(),
            lookup: lookup.clone(),
            cycle,
            ranks: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::default(),
            Box::new(session.clone()),
            Arc::new(zone()),
            "shared".into(),
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
        let pass_scope = passes[0].album.selection_scope();
        assert_eq!(
            pass_scope,
            json!({
                "zone":zone(),"name":"",
                "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
                "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
                "query_filter":null,"container_id":null,"cross_zone_sources":[]
            })
        );
        let expected_pass = format!(
            "{:x}",
            sha2::Sha256::digest(
                json!({"kind":"Unfiled","scope":pass_scope,"excludes":[]})
                    .to_string()
                    .as_bytes()
            )
        );
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
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
            result.stats.downloaded,
            if cycle == 0 { 1 } else { 0 },
            "only the independent new file may download"
        );
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap().as_deref(),
            Some(format!("scale-after-{}", cycle + 1).as_str())
        );
        assert_eq!(
            db.get_metadata(&format!("sync_token:{ZONE}"))
                .await
                .unwrap()
                .as_deref(),
            Some(OLD_TOKEN)
        );
        if cycle == 0 {
            assert!(
                session.ranks.load(Ordering::SeqCst) > 0,
                "baseline must enumerate"
            );
        }
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let pending:i64=conn.query_row("SELECT COUNT(*) FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND status IN ('pending','failed')",[ZONE,"asset-ambiguous","original"],|r|r.get(0)).unwrap();
        assert_eq!(pending, 1, "ambiguous current identity must remain visible");
        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND status='downloaded'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_count as usize, count + 1);
        let mappings:i64=conn.query_row("SELECT COUNT(*) FROM asset_master_mappings WHERE library='PrimarySync' AND master_record_name='legacy-ambiguous'",[],|r|r.get(0)).unwrap();
        assert_eq!(mappings, 2);
        let mut mapping_stmt = conn.prepare(
            "SELECT asset_record_name,master_record_name FROM asset_master_mappings WHERE library='PrimarySync' AND master_record_name='legacy-ambiguous' ORDER BY asset_record_name"
        ).unwrap();
        let mapping_rows = mapping_stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            mapping_rows,
            vec![
                ("history-a".into(), "legacy-ambiguous".into()),
                ("history-b".into(), "legacy-ambiguous".into())
            ]
        );
        let guessed:i64=conn.query_row("SELECT COUNT(*) FROM legacy_master_state_owners WHERE library='PrimarySync' AND master_record_name='legacy-ambiguous'",[],|r|r.get(0)).unwrap();
        assert_eq!(guessed, 0);
        let debt:i64=conn.query_row("SELECT COUNT(*) FROM metadata_capture_retries WHERE library='PrimarySync' AND asset_id='legacy-ambiguous'",[],|r|r.get(0)).unwrap();
        assert_eq!(debt, 1);
        let account = baseline_owner();
        let (account_key, provider_key) = account.authenticated_keys().unwrap();
        let expected_scope=json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"shared","zone":zone()}).to_string();
        use rusqlite::types::Value as SqlValue;
        let mut stmt=conn.prepare("SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum FROM download_receipts WHERE library=?1 ORDER BY child").unwrap();
        let receipts = stmt
            .query_map([ZONE], |row| {
                (0..14)
                    .map(|i| row.get::<_, SqlValue>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            receipts.len(),
            count + 1,
            "every qualified item and new item gets a receipt; ambiguous gets none"
        );
        let by_child: BTreeMap<_, _> = receipts
            .iter()
            .map(|r| match &r[4] {
                SqlValue::Text(child) => (child.as_str(), r),
                _ => panic!("receipt child must be text"),
            })
            .collect();
        assert_eq!(by_child.len(), receipts.len());
        for (id, _, file, bytes, mtime, provider, local) in &entries[..count] {
            let child = format!("asset-{id}");
            let row = &by_child[child.as_str()];
            let native = crate::state::db::provider_selection::SelectionPath::from_path(file)
                .key()
                .unwrap();
            let expected = vec![
                SqlValue::Text(account_key.into()),
                SqlValue::Text(provider_key.into()),
                SqlValue::Text(expected_scope.clone()),
                SqlValue::Text(ZONE.into()),
                SqlValue::Text(child.clone()),
                SqlValue::Text(id.clone()),
                SqlValue::Text(child),
                SqlValue::Text(expected_pass.clone()),
                SqlValue::Text("original".into()),
                SqlValue::Text(provider.clone()),
                SqlValue::Integer(i64::try_from(bytes.len()).unwrap()),
                SqlValue::Text(native),
                SqlValue::Text(local.clone()),
                SqlValue::Text(local.clone()),
            ];
            assert_eq!(*row, &expected, "scoped current receipt mismatch");
            assert_eq!(std::fs::read(file).unwrap(), *bytes);
            assert_eq!(std::fs::metadata(file).unwrap().modified().unwrap(), *mtime);
        }
        let new = by_child
            .get("asset-new")
            .expect("independent new receipt must exist");
        let new_path = root.join("new_YXNzZXQ.JPG");
        let new_hash = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(MEDIA));
        let new_expected = vec![
            SqlValue::Text(account_key.into()),
            SqlValue::Text(provider_key.into()),
            SqlValue::Text(expected_scope.clone()),
            SqlValue::Text(ZONE.into()),
            SqlValue::Text("asset-new".into()),
            SqlValue::Text("new".into()),
            SqlValue::Text("asset-new".into()),
            SqlValue::Text(expected_pass.clone()),
            SqlValue::Text("original".into()),
            SqlValue::Text(super::generation(3)),
            SqlValue::Integer(i64::try_from(MEDIA.len()).unwrap()),
            SqlValue::Text(
                crate::state::db::provider_selection::SelectionPath::from_path(&new_path)
                    .key()
                    .unwrap(),
            ),
            SqlValue::Text(new_hash.clone()),
            SqlValue::Text(new_hash),
        ];
        assert_eq!(*new, &new_expected);
        assert_eq!(
            historical_rows(&db_path),
            original_history,
            "historical asset/path/metadata debt changed"
        );
        assert!(!by_child.contains_key("asset-ambiguous"));
        assert!(
            by_child.contains_key("asset-new"),
            "independent new file must complete"
        );
        let (_, _, amb_path, amb_bytes, amb_mtime, _, _) = entries.last().unwrap();
        assert_eq!(std::fs::read(amb_path).unwrap(), *amb_bytes);
        assert_eq!(
            std::fs::metadata(amb_path).unwrap().modified().unwrap(),
            *amb_mtime
        );
        let mut tree = BTreeMap::new();
        for entry in std::fs::read_dir(&root).unwrap() {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            tree.insert(
                entry.file_name().into_string().unwrap(),
                std::fs::read(entry.path()).unwrap(),
            );
        }
        assert_eq!(tree.len(), count + 2);
        for (_, _, path, bytes, _, _, _) in &entries {
            assert_eq!(&tree[path.file_name().unwrap().to_str().unwrap()], bytes);
        }
        assert_eq!(&tree["new_YXNzZXQ.JPG"], MEDIA);
        if let Some(previous) = &receipt_snapshot {
            assert_eq!(&receipts, previous);
        } else {
            receipt_snapshot = Some(receipts);
        }
        let requests = server.received_requests().await.unwrap();
        let get_count = |name: &str| {
            requests
                .iter()
                .filter(|r| r.method.as_str() == "GET" && r.url.path() == name)
                .count()
        };
        assert_eq!(
            (
                get_count("/qualified"),
                get_count("/ambiguous"),
                get_count("/new")
            ),
            (0, 0, 1)
        );
        if cycle > 0 {
            assert_eq!(
                session.ranks.load(Ordering::SeqCst),
                0,
                "quiet cycles must not re-inventory"
            );
        }
        eprintln!(
            "SCALE_CYCLE_REACHED count={count} cycle={cycle} elapsed_ms={} rank_requests={} receipts={} pending={} downloaded={} db_bytes={} peak_kib={:?} qualified_gets=0 ambiguous_gets=0 new_gets=1",
            start.elapsed().as_millis(),
            session.ranks.load(Ordering::SeqCst),
            count + 1,
            pending,
            result.stats.downloaded,
            db_bytes(&db_path),
            peak_kib()
        );
    }
    server.verify().await;
    eprintln!(
        "SCALE_FINISHED count={count} total_ms={} peak_kib={:?}",
        total.elapsed().as_millis(),
        peak_kib()
    );
}
