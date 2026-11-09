//! Real cycle proof for an unprepared shared-original family.
//!
//! The historical family keeps its raw invalid local hash, absent source,
//! mappings, capture revision and retry evidence. It is never adopted,
//! duplicated or automatically certified. Two independent healthy current
//! children prove that legitimate admitted work still completes through the
//! owned normal cycle.
//!
//! Two explicit start states exercise the retained metadata-capture ambiguity:
//! a deadline that is still in the future, and a prior explicit refusal whose
//! deadline is already due. The due fixture reconstructs that prior refusal
//! before measurement; the cycle must preserve it as history, not invent a new
//! refusal generation.

use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoAsset, PhotoLibrary, PhotosSession};
use crate::state;
use crate::state::db::account::AccountOwner;
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, album_count_response, full_album_page_with_download,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
    run_cycle_expected_date_dir,
};
use base64::Engine as _;
use serde_json::{Value, json};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug)]
enum RetainedAmbiguity {
    FutureDeferred,
    AlreadyDue,
}

#[derive(Clone, Debug)]
struct Session {
    records: Vec<Value>,
    inventory: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request
            .get("query")
            .and_then(|query| query.get("recordType"))
            .and_then(Value::as_str)
            == Some("CheckIndexingState")
        {
            return Ok(json!({"records":[{
                "recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED","type":"STRING"}}
            }]}));
        }
        if url.contains("/internal/records/query/batch?") {
            let current_children = self
                .records
                .iter()
                .filter(|record| record["recordType"] == "CPLAsset")
                .count();
            return Ok(album_count_response(current_children as u64));
        }
        if url.contains("/records/query?") {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            // Album queries always see the complete current inventory.
            let records = if offset == 0 {
                self.records.clone()
            } else {
                Vec::new()
            };
            // A complete album query carries the zone token, as the real
            // provider and the existing positive fixture do.
            return Ok(json!({"records":records,"syncToken":"after"}));
        }
        if url.contains("/changes/zone?") {
            let cursor = request["zones"][0]["syncToken"].as_str();
            if cursor != Some("after") {
                self.inventory.fetch_add(1, Ordering::SeqCst);
            }
            // A no-cursor full scan and the first replay from the retained
            // "before" cursor both carry the complete current inventory. Only
            // the true after-cursor delta is empty, which is the quiet reopen.
            let records = if cursor == Some("after") {
                Vec::new()
            } else {
                self.records.clone()
            };
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                "syncToken":"after","moreComing":false,"records":records}]}),
            );
        }
        assert!(url.contains("/records/lookup?"), "unexpected request {url}");
        let requested = request["records"].as_array().unwrap();
        let records: Vec<_> = self
            .records
            .iter()
            .filter(|record| {
                requested
                    .iter()
                    .any(|id| id["recordName"] == record["recordName"])
            })
            .cloned()
            .collect();
        Ok(json!({"records":records}))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "baseline@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-baseline"}})).unwrap(),
    )
    .unwrap()
}

fn primary_zone() -> Value {
    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})
}

fn file_proof(
    path: &Path,
) -> (
    crate::fs_util::FileIdentity,
    u64,
    std::time::SystemTime,
    Vec<u8>,
) {
    let file = std::fs::File::open(path).unwrap();
    (
        crate::fs_util::file_identity(&file).unwrap(),
        crate::fs_util::file_link_count(&file).unwrap(),
        file.metadata().unwrap().modified().unwrap(),
        std::fs::read(path).unwrap(),
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn provider_signature(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    let mut value = vec![1];
    value.extend_from_slice(&hash.finalize());
    base64::engine::general_purpose::STANDARD.encode(value)
}

fn rows(database: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(database).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let count = statement.column_count();
    statement
        .query_map([], |row| {
            (0..count)
                .map(|index| row.get(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

/// Frozen historical proof columns. Status, retry timing bookkeeping and
/// current-observation timestamps stay outside this snapshot because the
/// negative family may remain visibly pending or failed.
fn historical_proof(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    [
        "SELECT library,id,version_size,checksum,created_at,added_at,size_bytes,media_type,source,is_deleted \
         FROM assets WHERE library='PrimarySync' AND id IN ('child-0-unique','child-1-unique') ORDER BY id,version_size",
        "SELECT library,id,version_size,checksum,created_at,added_at,size_bytes,media_type,source,is_deleted,local_path,local_checksum \
         FROM assets WHERE library='PrimarySync' AND id='legacy-master' AND version_size='original'",
        "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id IN ('legacy-master','child-0-unique','child-1-unique') AND local_checksum IN ('historical-local','legacy-local') ORDER BY id,version_size,local_path",
        "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE library='PrimarySync' AND asset_record_name IN ('child-0-unique','child-1-unique') ORDER BY asset_record_name",
        "SELECT library,asset_id,revision FROM asset_metadata_capture_revisions WHERE library='PrimarySync' AND asset_id IN ('legacy-master','child-0-unique','child-1-unique') ORDER BY asset_id",
        "SELECT library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at FROM metadata_capture_retries WHERE library='PrimarySync' ORDER BY asset_id,target_revision",
        "SELECT library,active_revision,pending_revision,failed_assets,last_error FROM metadata_capture_state WHERE library='PrimarySync'",
        "SELECT * FROM legacy_master_state_owners WHERE library='PrimarySync' ORDER BY master_record_name",
        "SELECT * FROM unattributed_legacy WHERE library='PrimarySync' ORDER BY asset_id",
    ]
    .into_iter()
    .flat_map(|sql| rows(database, sql))
    .collect()
}

fn refusal_fields(database: &Path) -> Vec<rusqlite::types::Value> {
    rows(
        database,
        "SELECT r.target_revision,r.attempts,r.generation,r.last_attempt_at,r.next_retry_at,s.pending_revision,s.failed_assets,s.last_error,v.revision \
         FROM metadata_capture_retries r JOIN metadata_capture_state s ON s.library=r.library \
         JOIN asset_metadata_capture_revisions v ON v.library=r.library AND v.asset_id=r.asset_id \
         WHERE r.library='PrimarySync' AND r.asset_id='legacy-master'",
    )
    .remove(0)
}

fn old_path_claims(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    rows(
        database,
        "SELECT id,local_path,local_checksum,source_checksum,download_checksum FROM asset_metadata_paths \
         WHERE library='PrimarySync' AND id IN ('legacy-master','child-0-unique','child-1-unique') \
           AND local_checksum IN ('historical-local','legacy-local') ORDER BY id,version_size",
    )
}

fn legacy_master_path_rows(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    rows(
        database,
        "SELECT id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum \
         FROM asset_metadata_paths WHERE library='PrimarySync' AND id='legacy-master' \
         ORDER BY version_size,local_path",
    )
}

fn current_path_claims(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    rows(
        database,
        "SELECT p.id,p.version_size,p.local_path,p.provider_checksum,p.local_checksum,p.source_checksum,p.download_checksum,a.checksum,a.size_bytes,a.download_checksum \
         FROM asset_metadata_paths p JOIN assets a ON a.library=p.library AND a.id=p.id AND a.version_size=p.version_size \
         WHERE p.library='PrimarySync' AND p.id IN ('asset-legacy-master','child-0-unique','child-1-unique') \
           AND p.local_checksum NOT IN ('historical-local','legacy-local') ORDER BY p.id,p.version_size",
    )
}

fn durable_key_sets(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    [
        "SELECT library,id,version_size FROM assets WHERE status='downloaded' ORDER BY library,id,version_size",
        "SELECT id FROM provider_active_generations ORDER BY id",
        "SELECT generation,pass_key,child FROM provider_active_decisions ORDER BY generation,pass_key,child",
        "SELECT generation,pass_key,child,version_size,path FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path",
        "SELECT count(*) FROM assets WHERE status='downloaded'",
        "SELECT count(*) FROM provider_active_generations",
        "SELECT count(*) FROM provider_active_decisions",
        "SELECT count(*) FROM provider_active_destinations",
    ]
    .into_iter()
    .flat_map(|sql| rows(database, sql))
    .collect()
}

fn pass_key(pass: &AlbumPass) -> String {
    let mut excludes: Vec<_> = pass.exclude_ids.iter().collect();
    excludes.sort_unstable();
    format!(
        "{:x}",
        Sha256::digest(
            json!({
                "kind":format!("{:?}",pass.kind),
                "scope":pass.album.selection_scope(),
                "excludes":excludes
            })
            .to_string()
            .as_bytes()
        )
    )
}

fn active_destinations(database: &Path, child: &str) -> Vec<Vec<rusqlite::types::Value>> {
    rows(
        database,
        &format!(
            "SELECT d.generation,d.pass_key,g.scope,g.sealed,g.checkpoint_ready,g.account_key,g.provider_key,d.asset_id,d.library,d.child,d.master,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,d.source_checksum,d.local_checksum \
             FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation \
             WHERE d.library='PrimarySync' AND d.child='{child}' AND d.version_size='original' ORDER BY d.generation"
        ),
    )
}

fn receipts_for(database: &Path, child: &str) -> Vec<Vec<rusqlite::types::Value>> {
    rows(
        database,
        &format!(
            "SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash \
             FROM download_receipts WHERE library='PrimarySync' AND child='{child}' AND version_size='original' ORDER BY native_path"
        ),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the complete healthy receipt is asserted field by field"
)]
fn assert_healthy_receipt(
    row: &[rusqlite::types::Value],
    account_key: &str,
    provider_key: &str,
    scope: &str,
    child: &str,
    master: &str,
    pass: &str,
    checksum: &str,
    bytes: &[u8],
    native: &Path,
) {
    use rusqlite::types::Value as SqlValue;
    let digest = sha256_hex(bytes);
    let expected = vec![
        SqlValue::Text(account_key.into()),
        SqlValue::Text(provider_key.into()),
        SqlValue::Text(scope.into()),
        SqlValue::Text("PrimarySync".into()),
        SqlValue::Text(child.into()),
        SqlValue::Text(master.into()),
        SqlValue::Text(child.into()),
        SqlValue::Text(pass.into()),
        SqlValue::Text("original".into()),
        SqlValue::Text(checksum.into()),
        SqlValue::Integer(i64::try_from(bytes.len()).unwrap()),
        SqlValue::Text(
            crate::state::db::provider_selection::SelectionPath::from_path(native)
                .key()
                .unwrap(),
        ),
        SqlValue::Text(digest.clone()),
        SqlValue::Text(digest),
    ];
    assert_eq!(
        &row[..14],
        expected.as_slice(),
        "incorrect healthy current receipt"
    );
    assert!(
        matches!(&row[14], SqlValue::Text(proof)
            if proof.len() == 64
                && proof.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
        "healthy current receipt proof hash must be 64 lowercase hex characters"
    );
}

async fn run_case(ambiguity: RetainedAmbiguity) {
    for retained_provider_spelling in [false, true] {
        Box::pin(run_case_with_spelling(
            ambiguity,
            retained_provider_spelling,
        ))
        .await;
    }
}

async fn run_case_with_spelling(ambiguity: RetainedAmbiguity, retained_provider_spelling: bool) {
    use base64::Engine as _;
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };

    let server = crate::start_wiremock_or_skip!();
    let bytes = [
        0xff, 0xd8, 0xff, 0xe0, 0, 0x10, 0x4a, 0x46, 0x49, 0x46, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0xff,
        0xd9,
    ];
    let mut checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    {
        // The retained shared original is an unsupported provider signature,
        // not a current positive proof.
        let mut decoded = base64::engine::general_purpose::STANDARD
            .decode(&checksum)
            .unwrap();
        decoded[0] = 4;
        checksum = base64::engine::general_purpose::STANDARD.encode(decoded);
    }
    let mut healthy_a = bytes.to_vec();
    healthy_a.extend_from_slice(b"healthy-current-a");
    let mut healthy_b = bytes.to_vec();
    healthy_b.extend_from_slice(b"healthy-current-b-with-distinct-size");
    assert_ne!(healthy_a.len(), healthy_b.len());
    assert_ne!(
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&healthy_a)),
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&healthy_b))
    );
    for (endpoint, body, expected) in [
        ("/child.jpg", bytes.to_vec(), 3),
        ("/healthy-a.jpg", healthy_a.clone(), 1),
        ("/healthy-b.jpg", healthy_b.clone(), 1),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(expected)
            .mount(&server)
            .await;
    }
    let page = full_album_page_with_download(
        "PrimarySync",
        "legacy-master",
        "after",
        &format!("{}/child.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    );
    let mut legacy_page = page["records"].clone();
    legacy_page[0]["fields"]["filenameEnc"] = json!({"value":"photo.jpg","type":"STRING"});
    let provider_asset = PhotoAsset::from_records(
        serde_json::from_value(legacy_page[0].clone()).unwrap(),
        &serde_json::from_value(legacy_page[1].clone()).unwrap(),
    );
    let names = ["child-0-unique", "child-1-unique"];
    // The original master and both historical children are offered alongside
    // the separate healthy current children. The old protected original is
    // not adoption authority; the run must classify the current opportunity.
    let mut records = vec![legacy_page[0].clone(), legacy_page[1].clone()];
    for name in names {
        let mut child = legacy_page[1].clone();
        child["recordName"] = json!(name);
        records.push(child);
    }
    let healthy = [
        (
            "healthy-a",
            "healthy-a.JPG",
            healthy_a.as_slice(),
            "/healthy-a.jpg",
        ),
        (
            "healthy-b",
            "healthy-b.JPG",
            healthy_b.as_slice(),
            "/healthy-b.jpg",
        ),
    ];
    for (master, filename, content, endpoint) in healthy {
        let mut pair = full_album_page_with_download(
            "PrimarySync",
            master,
            "after",
            &format!("{}{endpoint}", server.uri()),
            content.len() as u64,
            &provider_signature(content),
        )["records"]
            .as_array()
            .unwrap()
            .clone();
        pair[0]["fields"]["filenameEnc"] = json!({"value":filename,"type":"STRING"});
        records.extend(pair);
    }

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    let destination = media.join(run_cycle_expected_date_dir());
    std::fs::create_dir_all(&destination).unwrap();
    let config = make_run_cycle_config();
    let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
    let provider_filename = provider_asset
        .filename()
        .expect("provider master must render the protected filename");
    assert_eq!(
        provider_filename, "photo.jpg",
        "the provider filename, not a decoded fixture constant, names the protected path"
    );
    let rendered_filename = provider_asset
        .versions()
        .first()
        .map(|(_, version)| {
            crate::download::paths::map_filename_extension(provider_filename, &version.asset_type)
        })
        .expect("provider master must have a version");
    assert_eq!(rendered_filename, "photo.JPG");
    let original_filename = if retained_provider_spelling {
        provider_filename
    } else {
        &rendered_filename
    };
    let original = destination.join(original_filename);
    let sidecar = destination.join(format!("{original_filename}.xmp"));
    std::fs::write(&original, bytes).unwrap();
    std::fs::write(&sidecar, b"legacy sidecar: never replace").unwrap();
    let original_mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    let sidecar_mtime = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
    let original_proof = file_proof(&original);
    let sidecar_proof = file_proof(&sidecar);
    assert_eq!(
        crate::download::paths::local_download_path(
            &media,
            &config.download.folder_structure,
            &date,
            &rendered_filename,
            None,
        ),
        destination.join(&rendered_filename),
        "normal filename rendering must use the expected extension"
    );
    assert_eq!(
        original.file_name().unwrap(),
        std::ffi::OsStr::new(original_filename),
        "the protected path uses the selected retained spelling"
    );

    let owned = owner();
    {
        let db = state::SqliteStateDb::open_owned(&database, &owned)
            .await
            .unwrap();
        let mut metadata = state::AssetMetadata::default();
        metadata.refresh_hash();
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("legacy-master")
                .filename(&rendered_filename)
                .created_at(date)
                .added_at(date)
                .checksum(&checksum)
                .size(bytes.len() as u64)
                .metadata(metadata.clone())
                .build(),
        )
        .await
        .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "legacy-master",
            "original",
            &original,
            "legacy-local",
            None,
        )
        .await
        .unwrap();
        for child in names {
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new(child)
                    .filename(&rendered_filename)
                    .created_at(date)
                    .added_at(date)
                    .checksum(&checksum)
                    .size(bytes.len() as u64)
                    .metadata(metadata.clone())
                    .build(),
            )
            .await
            .unwrap();
            // The unprepared shared-original state: the child is recorded as
            // downloaded only at the protected original, with a historical
            // receipt, an invalid local hash and no source-backed path.
            db.mark_downloaded(
                "PrimarySync",
                child,
                "original",
                &original,
                "historical-local",
                None,
            )
            .await
            .unwrap();
            db.upsert_asset_master_mapping("PrimarySync", child, "legacy-master")
                .await
                .unwrap();
            db.set_metadata_capture_revision_for_test("PrimarySync", child, 1);
        }
        {
            let conn = rusqlite::Connection::open(&database).unwrap();
            for id in std::iter::once("legacy-master").chain(names.iter().copied()) {
                conn.execute(
                    "INSERT INTO asset_albums(library,asset_id,album_name,source)
                     VALUES ('PrimarySync',?1,'TestAlbum','icloud')",
                    rusqlite::params![id],
                )
                .unwrap();
            }
        }
        db.set_metadata("sync_token:PrimarySync", "before")
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "legacy-master", 0);
        db.begin_metadata_capture_revision("PrimarySync", state::METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        let candidate = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(candidate.asset_id, "legacy-master");
        assert!(
            db.defer_metadata_capture_ambiguity(&candidate, 1)
                .await
                .unwrap()
        );
    }
    if matches!(ambiguity, RetainedAmbiguity::AlreadyDue) {
        // Fixture history only: normal time passed and an explicit prior
        // refusal exists. The run must preserve this evidence, not create a
        // second refusal generation.
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute(
            "UPDATE metadata_capture_retries SET last_attempt_at=0,next_retry_at=1
             WHERE asset_id='legacy-master'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE metadata_capture_state SET pending_revision=1,failed_assets=1,
             last_error='multiple provider children matched durable catalogue evidence'
             WHERE library='PrimarySync'",
            [],
        )
        .unwrap();
    }

    let frozen_before = historical_proof(&database);
    let refusal_before = refusal_fields(&database);
    let old_paths_before = old_path_claims(&database);
    assert_eq!(old_paths_before.len(), 3, "three original path proofs");
    let legacy_master_paths_before = legacy_master_path_rows(&database);
    if matches!(ambiguity, RetainedAmbiguity::AlreadyDue) {
        let state_error = rows(
            &database,
            "SELECT last_error FROM metadata_capture_state WHERE library='PrimarySync'",
        );
        let error = match &state_error[0][0] {
            rusqlite::types::Value::Text(error) => Some(error.as_str()),
            _ => None,
        };
        assert_eq!(
            error,
            Some("multiple provider children matched durable catalogue evidence"),
            "the due case starts from an explicit prior refusal"
        );
    }

    let inventory = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        inventory.load(Ordering::SeqCst),
        0,
        "initial inventory count"
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let expected_scope = json!({"format":1,"realm":"com","container":"com.apple.photos.cloud",
        "environment":"production","database":"private","zone":primary_zone()})
    .to_string();
    let (account_key, provider_key) = owned.authenticated_keys().unwrap();

    let run_cycle = |_cycle: usize| {
        let database = &database;
        let media = &media;
        let config = &config;
        let shared_session = &shared_session;
        let inventory = inventory.clone();
        let records = records.clone();
        let owned = owned.clone();
        async move {
            let inner = Arc::new(
                state::SqliteStateDb::open_owned(database, &owned)
                    .await
                    .unwrap(),
            );
            let db: Arc<dyn download::DownloadStore> = inner.clone();
            let base_builder = make_run_cycle_download_config_builder(media, db.clone());
            let builder = |mode, excluded, groups, library| {
                let mut built = base_builder(mode, excluded, groups, library);
                // The reported live recovery ran with metadata writers disabled.
                Arc::make_mut(&mut built).metadata = Default::default();
                built
            };
            let provider = Session { records, inventory };
            let mut library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::new(std::collections::HashMap::new()),
                Box::new(provider.clone()),
                Arc::new(primary_zone()),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
                inner.clone(),
                owned.clone(),
                "com",
            ));
            let passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            let expected_pass = pass_key(&passes[0]);
            let mut primary = make_run_cycle_library_state_with_passes(
                "PrimarySync",
                "sync_token:PrimarySync",
                passes,
            );
            primary.library = library;
            let result = crate::sync_cycle::run_cycle(
                &[&primary],
                config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                shared_session,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            (inner, result, expected_pass)
        }
    };

    let cycle_count = if matches!(ambiguity, RetainedAmbiguity::AlreadyDue) {
        4
    } else {
        3
    };
    let mut expected_healthy_paths = None;
    for cycle in 0..cycle_count {
        let (inner, result, expected_pass) = run_cycle(cycle).await;
        {
            let requests = server.received_requests().await.unwrap();
            let gets: Vec<_> = requests
                .iter()
                .filter(|request| request.method.as_str() == "GET")
                .map(|request| request.url.path().to_owned())
                .collect();
            let selected = inner.get_downloaded_page(0, 20).await.unwrap();
            let selected_rows: Vec<_> = selected
                .iter()
                .map(|row| {
                    (
                        row.id.to_string(),
                        row.version_size.as_str().to_owned(),
                        row.local_path
                            .as_ref()
                            .map(|path| path.display().to_string()),
                        row.local_checksum.clone(),
                    )
                })
                .collect();
            eprintln!(
                "SELECTED_TRACE cycle={cycle} inventory={} stats_downloaded={} failed={} gets={gets:?} completed={selected_rows:?}",
                inventory.load(Ordering::SeqCst),
                result.stats.downloaded,
                result.failed_count
            );
        }
        assert_eq!(
            std::fs::read(&original).unwrap(),
            bytes,
            "cycle {cycle}: original bytes"
        );
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            original_mtime,
            "cycle {cycle}: original mtime"
        );
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"legacy sidecar: never replace",
            "cycle {cycle}: original sidecar"
        );
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
            sidecar_mtime,
            "cycle {cycle}: sidecar mtime"
        );
        assert_eq!(
            file_proof(&original),
            original_proof,
            "cycle {cycle}: original filesystem identity/link/mtime/bytes"
        );
        assert_eq!(
            file_proof(&sidecar),
            sidecar_proof,
            "cycle {cycle}: sidecar filesystem identity/link/mtime/bytes"
        );
        assert_eq!(
            historical_proof(&database),
            frozen_before,
            "cycle {cycle}: frozen historical proof"
        );
        assert_eq!(
            refusal_fields(&database),
            refusal_before,
            "cycle {cycle}: prior refusal and retry generation"
        );
        assert_eq!(
            old_path_claims(&database),
            old_paths_before,
            "cycle {cycle}: original path proofs stay exact"
        );
        assert_eq!(
            legacy_master_path_rows(&database),
            legacy_master_paths_before,
            "cycle {cycle}: all legacy-master path rows stay exact"
        );
        let current_claims = current_path_claims(&database);
        let protected_occupies_base = original == destination.join(&rendered_filename);
        let selected_native = |state_id: &str| {
            if protected_occupies_base || state_id != "asset-legacy-master" {
                destination.join(crate::download::paths::insert_asset_identity_suffix(
                    &rendered_filename,
                    state_id,
                ))
            } else {
                destination.join(&rendered_filename)
            }
        };
        let digest = sha256_hex(&bytes);
        let mut expected_claims = std::collections::BTreeMap::new();
        expected_claims.insert(
            "asset-legacy-master".to_owned(),
            (
                "original".to_owned(),
                selected_native("asset-legacy-master"),
                checksum.clone(),
                i64::try_from(bytes.len()).unwrap(),
                digest.clone(),
                digest.clone(),
                digest.clone(),
                digest.clone(),
                checksum.clone(),
            ),
        );
        if cycle > 0 {
            for child in names {
                expected_claims.insert(
                    child.to_owned(),
                    (
                        "original".to_owned(),
                        selected_native(child),
                        checksum.clone(),
                        i64::try_from(bytes.len()).unwrap(),
                        digest.clone(),
                        digest.clone(),
                        digest.clone(),
                        digest.clone(),
                        checksum.clone(),
                    ),
                );
            }
        }
        let mut actual_claims = std::collections::BTreeMap::new();
        for row in &current_claims {
            use rusqlite::types::Value as SqlValue;
            let (
                SqlValue::Text(id),
                SqlValue::Text(version),
                SqlValue::Text(path),
                SqlValue::Text(provider),
                SqlValue::Text(local),
                SqlValue::Text(source),
                SqlValue::Text(path_download),
                SqlValue::Text(asset_checksum),
                SqlValue::Integer(size),
                SqlValue::Text(asset_download),
            ) = (
                &row[0], &row[1], &row[2], &row[3], &row[4], &row[5], &row[6], &row[7], &row[8],
                &row[9],
            )
            else {
                panic!("cycle {cycle}: current claim columns are malformed");
            };
            let current_path = std::path::PathBuf::from(path);
            assert_ne!(
                current_path, original,
                "cycle {cycle}: current proof never relabels the protected original"
            );
            let metadata = std::fs::symlink_metadata(&current_path).unwrap();
            assert!(
                metadata.file_type().is_file(),
                "cycle {cycle}: current proof must be a regular file"
            );
            let proof = file_proof(&current_path);
            assert_ne!(
                proof.0, original_proof.0,
                "cycle {cycle}: current proof must not alias the protected original inode"
            );
            assert_eq!(proof.1, 1, "cycle {cycle}: current proof has one link only");
            assert_eq!(proof.3, bytes, "cycle {cycle}: current publication bytes");
            assert!(
                actual_claims
                    .insert(
                        id.clone(),
                        (
                            version.clone(),
                            current_path,
                            provider.clone(),
                            *size,
                            local.clone(),
                            source.clone(),
                            path_download.clone(),
                            asset_download.clone(),
                            asset_checksum.clone(),
                        ),
                    )
                    .is_none(),
                "cycle {cycle}: one current claim per selected identity"
            );
        }
        assert_eq!(
            actual_claims, expected_claims,
            "cycle {cycle}: exact selected current path/provider/size/digest map"
        );
        assert_eq!(
            inventory.load(Ordering::SeqCst),
            1,
            "cycle {cycle}: exact catalog replay count"
        );
        assert!(
            inner
                .legacy_preservations("PrimarySync")
                .await
                .unwrap()
                .is_empty(),
            "cycle {cycle}: automatic certification is retired"
        );

        let page = inner.get_downloaded_page(0, 20).await.unwrap();
        let mut healthy_paths = std::collections::BTreeMap::new();
        for (master, filename, content, _) in [
            ("healthy-a", "healthy-a.JPG", healthy_a.as_slice(), ""),
            ("healthy-b", "healthy-b.JPG", healthy_b.as_slice(), ""),
        ] {
            let child = format!("asset-{master}");
            let row = page
                .iter()
                .find(|row| row.id.as_ref() == child)
                .unwrap_or_else(|| panic!("cycle {cycle}: healthy child {child} missing"));
            let path = row.local_path.as_ref().expect("healthy path");
            assert_eq!(
                path,
                &destination.join(filename),
                "cycle {cycle}: exact native healthy path"
            );
            let metadata = std::fs::symlink_metadata(path).unwrap();
            assert!(
                metadata.file_type().is_file(),
                "cycle {cycle}: healthy proof must be a regular file"
            );
            let proof = file_proof(path);
            assert_ne!(
                proof.0, original_proof.0,
                "cycle {cycle}: healthy proof must not alias the protected original inode"
            );
            assert_eq!(proof.1, 1, "cycle {cycle}: healthy proof has one link");
            assert_eq!(proof.3, content, "cycle {cycle}: healthy bytes");
            let digest = sha256_hex(content);
            assert_eq!(row.local_checksum.as_deref(), Some(digest.as_str()));
            healthy_paths.insert(child, path.clone());
        }
        if let Some(expected) = &expected_healthy_paths {
            assert_eq!(
                &healthy_paths, expected,
                "cycle {cycle}: healthy paths stay stable"
            );
        } else {
            expected_healthy_paths = Some(healthy_paths);
        }

        for (master, filename, content, _) in [
            ("healthy-a", "healthy-a.JPG", healthy_a.as_slice(), ""),
            ("healthy-b", "healthy-b.JPG", healthy_b.as_slice(), ""),
        ] {
            let child = format!("asset-{master}");
            let native = destination.join(filename);
            let checksum = provider_signature(content);
            let receipts = receipts_for(&database, &child);
            assert_eq!(
                receipts.len(),
                1,
                "cycle {cycle}: exactly one healthy receipt for {child}"
            );
            assert_healthy_receipt(
                &receipts[0],
                account_key,
                provider_key,
                &expected_scope,
                &child,
                master,
                &expected_pass,
                &checksum,
                content,
                &native,
            );
            let destinations = active_destinations(&database, &child);
            assert_eq!(
                destinations.len(),
                1,
                "cycle {cycle}: one admitted healthy destination"
            );
            let destination = &destinations[0];
            use rusqlite::types::Value as SqlValue;
            assert!(
                matches!(&destination[0], SqlValue::Text(generation) if !generation.is_empty())
            );
            assert_eq!(destination[1], SqlValue::Text(expected_pass.clone()));
            assert_eq!(destination[2], SqlValue::Text(expected_scope.clone()));
            assert_eq!(destination[3], SqlValue::Integer(1));
            assert_eq!(destination[4], SqlValue::Integer(1));
            assert_eq!(destination[5], SqlValue::Text(account_key.into()));
            assert_eq!(destination[6], SqlValue::Text(provider_key.into()));
            assert_eq!(destination[7], SqlValue::Text(child.clone()));
            assert_eq!(destination[8], SqlValue::Text("PrimarySync".into()));
            assert_eq!(destination[9], SqlValue::Text(child.clone()));
            assert_eq!(destination[10], SqlValue::Text(master.into()));
            assert_eq!(destination[11], SqlValue::Text("original".into()));
            assert_eq!(
                destination[12],
                SqlValue::Text(
                    crate::state::db::provider_selection::SelectionPath::from_path(&native)
                        .key()
                        .unwrap()
                )
            );
            assert_eq!(destination[13], SqlValue::Text(checksum.clone()));
            assert_eq!(
                destination[14],
                SqlValue::Integer(i64::try_from(content.len()).unwrap())
            );
            assert_eq!(destination[15], SqlValue::Integer(1));
            assert_eq!(destination[16], SqlValue::Integer(1));
            assert_eq!(destination[17], SqlValue::Text(sha256_hex(content)));
            assert_eq!(destination[18], SqlValue::Text(sha256_hex(content)));
        }
        let selected_current = [
            ("asset-legacy-master", "legacy-master", true),
            ("child-0-unique", "legacy-master", cycle > 0),
            ("child-1-unique", "legacy-master", cycle > 0),
        ];
        let protected_occupies_base = original == destination.join(&rendered_filename);
        for (child, master, present) in selected_current {
            let expected_native = if protected_occupies_base || child != "asset-legacy-master" {
                destination.join(crate::download::paths::insert_asset_identity_suffix(
                    &rendered_filename,
                    child,
                ))
            } else {
                destination.join(&rendered_filename)
            };
            if !present {
                assert!(
                    page.iter().all(|row| row.id.as_ref() != child),
                    "cycle {cycle}: {child} is not current before its opportunity"
                );
                continue;
            }
            let row = page
                .iter()
                .find(|row| row.id.as_ref() == child)
                .unwrap_or_else(|| panic!("cycle {cycle}: selected child {child} missing"));
            assert_eq!(
                row.local_path.as_ref(),
                Some(&expected_native),
                "cycle {cycle}: exact selected native destination for {child}"
            );
            assert_eq!(std::fs::read(&expected_native).unwrap(), bytes);
            assert_eq!(
                row.local_checksum.as_deref(),
                Some(sha256_hex(&bytes).as_str())
            );
            let receipts = receipts_for(&database, child);
            assert_eq!(
                receipts.len(),
                1,
                "cycle {cycle}: exactly one selected receipt for {child}"
            );
            assert_healthy_receipt(
                &receipts[0],
                account_key,
                provider_key,
                &expected_scope,
                child,
                master,
                &expected_pass,
                &checksum,
                &bytes,
                &expected_native,
            );
            let destinations = active_destinations(&database, child);
            assert_eq!(
                destinations.len(),
                1,
                "cycle {cycle}: one admitted selected destination"
            );
            let destination = &destinations[0];
            use rusqlite::types::Value as SqlValue;
            assert!(
                matches!(&destination[0], SqlValue::Text(generation) if !generation.is_empty())
            );
            assert_eq!(destination[1], SqlValue::Text(expected_pass.clone()));
            assert_eq!(destination[2], SqlValue::Text(expected_scope.clone()));
            assert_eq!(destination[3], SqlValue::Integer(1));
            assert_eq!(destination[4], SqlValue::Integer(1));
            assert_eq!(destination[5], SqlValue::Text(account_key.into()));
            assert_eq!(destination[6], SqlValue::Text(provider_key.into()));
            assert_eq!(destination[7], SqlValue::Text(child.into()));
            assert_eq!(destination[8], SqlValue::Text("PrimarySync".into()));
            assert_eq!(destination[9], SqlValue::Text(child.into()));
            assert_eq!(destination[10], SqlValue::Text(master.into()));
            assert_eq!(destination[11], SqlValue::Text("original".into()));
            assert_eq!(
                destination[12],
                SqlValue::Text(
                    crate::state::db::provider_selection::SelectionPath::from_path(
                        &expected_native
                    )
                    .key()
                    .unwrap()
                )
            );
            assert_eq!(destination[13], SqlValue::Text(checksum.clone()));
            assert_eq!(
                destination[14],
                SqlValue::Integer(i64::try_from(bytes.len()).unwrap())
            );
            assert_eq!(destination[15], SqlValue::Integer(1));
            assert_eq!(destination[16], SqlValue::Integer(1));
            assert_eq!(destination[17], SqlValue::Text(sha256_hex(&bytes)));
            assert_eq!(destination[18], SqlValue::Text(sha256_hex(&bytes)));
        }
        let protected_key =
            crate::state::db::provider_selection::SelectionPath::from_path(&original)
                .key()
                .unwrap();
        assert!(
            rows(
                &database,
                &format!(
                    "SELECT id FROM download_receipts WHERE library='PrimarySync' AND native_path='{protected_key}'"
                ),
            )
            .is_empty(),
            "cycle {cycle}: no current receipt claims the protected original"
        );
        assert!(
            rows(
                &database,
                &format!(
                    "SELECT child FROM provider_active_destinations WHERE library='PrimarySync' AND path='{protected_key}'"
                ),
            )
            .is_empty(),
            "cycle {cycle}: no admitted destination claims the protected original"
        );

        let healthy_a_checksum = provider_signature(&healthy_a);
        let healthy_b_checksum = provider_signature(&healthy_b);
        assert_ne!(
            healthy_a_checksum, healthy_b_checksum,
            "healthy children must not share a descriptor"
        );
        assert_ne!(
            healthy_a.len(),
            healthy_b.len(),
            "distinct descriptors cannot claim one occupant"
        );

        assert_eq!(
            result.stats.downloaded,
            match cycle {
                0 => 3,
                1 => 2,
                _ => 0,
            },
            "cycle {cycle}: selected master, then two selected children, then quiet"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("after"),
            "cycle {cycle}: current checkpoint"
        );
        assert!(
            result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            result.failed_count,
            usize::from(cycle == 0) * 2,
            "cycle {cycle}: unresolved before the selected children complete: {result:?}"
        );
        assert_eq!(
            result.stats.skipped.retry_exhausted, 0,
            "cycle {cycle}: no current retry exhaustion: {result:?}"
        );
    }

    // Two actual owned reopen tails. Healthy completed work must stay quiet;
    // the historical negative debt must remain visible.
    let inventory_before_tails = inventory.load(Ordering::SeqCst);
    assert_eq!(
        inventory_before_tails, 1,
        "exact catalog replay count before tails"
    );
    let current_claims_before_tails = current_path_claims(&database);
    let receipts_before = rows(&database, "SELECT * FROM download_receipts ORDER BY id");
    let destinations_before = rows(
        &database,
        "SELECT * FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path",
    );
    let durable_keys_before = durable_key_sets(&database);
    for tail in 0..2 {
        let (inner, result, _) = run_cycle(cycle_count + tail).await;
        assert_eq!(result.stats.downloaded, 0, "tail {tail}: no transfers");
        assert_eq!(
            std::fs::read(&original).unwrap(),
            bytes,
            "tail {tail}: original bytes"
        );
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            original_mtime,
            "tail {tail}: original mtime"
        );
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"legacy sidecar: never replace",
            "tail {tail}: sidecar"
        );
        assert_eq!(
            file_proof(&original),
            original_proof,
            "tail {tail}: original filesystem identity/link/mtime/bytes"
        );
        assert_eq!(
            file_proof(&sidecar),
            sidecar_proof,
            "tail {tail}: sidecar filesystem identity/link/mtime/bytes"
        );
        assert_eq!(
            historical_proof(&database),
            frozen_before,
            "tail {tail}: historical proof"
        );
        assert_eq!(
            refusal_fields(&database),
            refusal_before,
            "tail {tail}: refusal evidence"
        );
        assert!(
            inner
                .legacy_preservations("PrimarySync")
                .await
                .unwrap()
                .is_empty(),
            "tail {tail}: no automatic certification"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("after"),
            "tail {tail}: checkpoint stays current"
        );
        assert_eq!(
            rows(&database, "SELECT * FROM download_receipts ORDER BY id"),
            receipts_before,
            "tail {tail}: receipts stay quiet"
        );
        assert_eq!(
            rows(
                &database,
                "SELECT * FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path"
            ),
            destinations_before,
            "tail {tail}: admitted destinations stay quiet"
        );
        assert_eq!(
            durable_key_sets(&database),
            durable_keys_before,
            "tail {tail}: completed asset and generation key sets do not grow"
        );
        assert_eq!(
            current_path_claims(&database),
            current_claims_before_tails,
            "tail {tail}: current path claim fields stay exact"
        );
        assert_eq!(
            legacy_master_path_rows(&database),
            legacy_master_paths_before,
            "tail {tail}: all legacy-master path rows stay exact"
        );
        assert_eq!(
            result.failed_count, 0,
            "tail {tail}: selected family completed cleanly: {result:?}"
        );
        assert_eq!(
            result.stats.skipped.retry_exhausted, 0,
            "tail {tail}: no current retry exhaustion: {result:?}"
        );
        assert_eq!(
            inventory.load(Ordering::SeqCst),
            inventory_before_tails,
            "tail {tail}: quiet reopen performs no catalog growth"
        );
    }

    server.verify().await;
}

#[tokio::test]
async fn run_cycle_unprepared_future_deferred_ambiguity_preserves_family() {
    Box::pin(run_case(RetainedAmbiguity::FutureDeferred)).await;
}

#[tokio::test]
async fn run_cycle_unprepared_due_ambiguity_preserves_family_and_keeps_refusal() {
    Box::pin(run_case(RetainedAmbiguity::AlreadyDue)).await;
}
