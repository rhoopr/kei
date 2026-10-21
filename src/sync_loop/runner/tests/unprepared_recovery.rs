//! Real cycle proof for an unprepared shared-original family. A normal provider
//! delta must keep the complete current inventory, and the historical-claim
//! consumer must never reassign the original family to a child.
//!
//! Two explicit start states exercise the retained metadata-capture ambiguity:
//! a deadline that is still in the future, and a
//! deadline that is already due because normal time passed before the run.
//!
//! Identity repair still refuses to choose between two current children that
//! share the master's durable version evidence, so the legacy revision stays 0.
//! Preservation does not depend on choosing an owner: the existing unattributed
//! path protects the original and sidecar, then certifies the family from the
//! complete provider inventory. The due state therefore advances the checkpoint
//! and keeps the refusal evidence rather than holding the original forever.

use crate::download;
use crate::icloud::photos::{PhotoAsset, PhotosSession};
use crate::state;
use crate::sync_loop::test_support::{
    RUN_CYCLE_ASSET_DATE_MS, album_count_response, full_album_page_with_download,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_album, make_shared_session_for_run_cycle,
    run_cycle_expected_date_dir,
};
use serde_json::{Value, json};
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
        if url.contains("/internal/records/query/batch?") {
            // The album page always reports the complete current child count.
            return Ok(album_count_response(self.records.len() as u64 - 1));
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
            if cursor.is_none() {
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

fn historical_rows(database: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(database).unwrap();
    let mut stmt = conn
        .prepare("SELECT library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size,source_checksum FROM asset_metadata_paths WHERE id!='legacy-master' AND local_checksum='historical-local' ORDER BY id")
        .unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn retry_row(database: &std::path::Path) -> Vec<rusqlite::types::Value> {
    let conn = rusqlite::Connection::open(database).unwrap();
    let mut stmt = conn
        .prepare("SELECT * FROM metadata_capture_retries WHERE asset_id='legacy-master'")
        .unwrap();
    let count = stmt.column_count();
    stmt.query_row([], |row| (0..count).map(|index| row.get(index)).collect())
        .unwrap()
}

fn owner_rows(database: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = rusqlite::Connection::open(database).unwrap();
    let mut stmt = conn
        .prepare("SELECT * FROM legacy_master_state_owners ORDER BY library,master_record_name")
        .unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
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
    use sha2::{Digest, Sha256};
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };

    let server = crate::start_wiremock_or_skip!();
    let bytes = [
        0xff, 0xd8, 0xff, 0xe0, 0, 0x10, 0x4a, 0x46, 0x49, 0x46, 0, 1, 0, 0, 1, 0, 1, 0, 0, 0xff,
        0xd9,
    ];
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    Mock::given(method("GET"))
        .and(path("/child.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(match ambiguity {
            RetainedAmbiguity::FutureDeferred => 2,
            // The due start state reattempts the retained identity; the two
            // independent children are still the only media transfers.
            RetainedAmbiguity::AlreadyDue => 2,
        })
        .mount(&server)
        .await;
    let page = full_album_page_with_download(
        "PrimarySync",
        "legacy-master",
        "after",
        &format!("{}/child.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    );
    let mut records = vec![page["records"][0].clone()];
    records[0]["fields"]["filenameEnc"] = json!({"value":"photo.jpg","type":"STRING"});
    let provider_asset = PhotoAsset::from_records(
        serde_json::from_value(records[0].clone()).unwrap(),
        &serde_json::from_value(page["records"][1].clone()).unwrap(),
    );
    let names = ["child-0-unique", "child-1-unique"];
    for name in names {
        let mut child = page["records"][1].clone();
        child["recordName"] = json!(name);
        records.push(child);
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
    // Normal path rendering maps the UTI extension to uppercase. The protected
    // original must live at the exact path the planner renders, not at a
    // case-only sibling that hides the collision on case-sensitive hosts.
    let rendered_filename = provider_asset
        .versions()
        .first()
        .map(|(_, version)| {
            crate::download::paths::map_filename_extension(provider_filename, &version.asset_type)
        })
        .expect("provider master must have a version");
    assert_eq!(rendered_filename, "photo.JPG");
    // Cover both the exact rendered path and the retained provider spelling.
    // The latter aliases the rendered path on case-insensitive filesystems.
    let original_filename = if retained_provider_spelling {
        provider_filename
    } else {
        &rendered_filename
    };
    let original = destination.join(original_filename);
    let sidecar = destination.join(format!("{original_filename}.xmp"));
    std::fs::write(&original, bytes).unwrap();
    std::fs::write(&sidecar, b"legacy sidecar: never replace").unwrap();
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

    {
        let db = state::SqliteStateDb::open(&database).await.unwrap();
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
            // receipt and no independent source-backed path.
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
                // This variant assumes stable recorded album membership.
                // New membership creates separate metadata debt; that debt
                // must not be fabricated or cleared by the recovery action.
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
        // The children are capture-complete, but the master keeps the deferred
        // legacy capture candidate that the existing historical fixture seeds.
        // This is what keeps the family discoverable without inventing
        // preservation or owner rows.
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
        if matches!(ambiguity, RetainedAmbiguity::AlreadyDue) {
            // Normal time passed before this run started. The retained capture
            // identity stays at the established legacy revision 0; only the
            // deferred deadline is already due. Fixture setup only: no run
            // action resets the deadline after a cycle.
            let conn = rusqlite::Connection::open(&database).unwrap();
            conn.execute(
                "UPDATE metadata_capture_retries SET last_attempt_at=0,next_retry_at=1
                 WHERE asset_id='legacy-master'",
                [],
            )
            .unwrap();
        }
    }

    let historical = historical_rows(&database);
    assert_eq!(historical.len(), names.len(), "initial historical receipts");
    let retry_before = retry_row(&database);
    let owners_before = owner_rows(&database);

    let inventory = Arc::new(AtomicUsize::new(0));
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let mut downloaded_paths = None;

    let run_cycle = |_cycle: usize| {
        let database = &database;
        let media = &media;
        let config = &config;
        let shared_session = &shared_session;
        let inventory = inventory.clone();
        let records = records.clone();
        async move {
            let inner = Arc::new(state::SqliteStateDb::open(database).await.unwrap());
            let db: Arc<dyn download::DownloadStore> = inner.clone();
            let base_builder = make_run_cycle_download_config_builder(media, db.clone());
            let builder = |mode, excluded, groups, library| {
                let mut built = base_builder(mode, excluded, groups, library);
                // The reported live recovery ran with metadata writers disabled.
                Arc::make_mut(&mut built).metadata = Default::default();
                built
            };
            // The provider session is the library's real source: the album
            // pass and the library-level preservation inventory must observe
            // the same records, exactly as the existing positive fixture does.
            let provider = Session { records, inventory };
            let mut primary = make_run_cycle_library_state_with_album(
                "PrimarySync",
                "sync_token:PrimarySync",
                crate::sync_loop::test_support::make_full_album_with_boxed_session(
                    "PrimarySync",
                    Box::new(provider.clone()),
                ),
            );
            primary.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(provider),
                "PrimarySync",
            );
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
            (inner, result)
        }
    };

    let cycle_count = if matches!(ambiguity, RetainedAmbiguity::AlreadyDue) {
        4
    } else {
        3
    };
    for cycle in 0..cycle_count {
        let (inner, result) = run_cycle(cycle).await;
        assert_eq!(
            std::fs::read(&original).unwrap(),
            bytes,
            "cycle {cycle}: original bytes"
        );
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"legacy sidecar: never replace",
            "cycle {cycle}: original sidecar"
        );
        assert_eq!(
            historical_rows(&database),
            historical,
            "cycle {cycle}: historical receipts must stay unchanged"
        );
        assert_eq!(
            owner_rows(&database),
            owners_before,
            "cycle {cycle}: historical owners must stay unchanged"
        );
        let presences = inner.legacy_preservations("PrimarySync").await.unwrap();
        let checkpoint = inner.get_metadata("sync_token:PrimarySync").await.unwrap();
        if cycle == 0 {
            assert_eq!(result.stats.downloaded, names.len(), "{result:?}");
            let mut paths = std::collections::HashSet::new();
            let page = inner.get_downloaded_page(0, 20).await.unwrap();
            for child in names {
                let row = page
                    .iter()
                    .find(|row| row.id.as_ref() == child)
                    .unwrap_or_else(|| panic!("child missing {child}"));
                let path = row.local_path.as_ref().unwrap();
                assert_ne!(path, &original, "shared original was adopted for {child}");
                assert!(paths.insert(path.clone()), "child aliases {path:?}");
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            downloaded_paths = Some(paths);
        } else {
            assert_eq!(
                result.stats.downloaded, 0,
                "cycle {cycle}: no duplicate work"
            );
            let mut paths = std::collections::HashSet::new();
            for child in names {
                let row = inner
                    .get_downloaded_page(0, 20)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|row| row.id.as_ref() == child)
                    .unwrap();
                paths.insert(row.local_path.clone().unwrap());
            }
            assert_eq!(
                downloaded_paths.as_ref(),
                Some(&paths),
                "cycle {cycle}: stable paths"
            );
        }
        if cycle == 0 {
            // The first complete inventory cannot certify yet: the family is
            // still a capture candidate, and identity repair has not run.
            assert!(
                presences.is_empty(),
                "cycle {cycle}: no preservation before the family inventory"
            );
            assert_eq!(
                checkpoint.as_deref(),
                Some("before"),
                "cycle {cycle}: the unprepared family holds the checkpoint"
            );
            assert_eq!(
                result.stats.sync_token_blocked_reason,
                Some("metadata_capture_repair_failed"),
                "cycle {cycle}: {result:?}"
            );
        } else {
            // The complete family inventory activates unattributed preservation
            // without assigning the original to either current child.
            assert_eq!(presences.len(), 1, "cycle {cycle}: one family original");
            assert_eq!(presences[0].asset_id, "legacy-master");
            assert_eq!(
                presences[0].active_generation,
                Some(1),
                "cycle {cycle}: the family proof is active"
            );
            assert_eq!(
                presences[0].files.len(),
                2,
                "cycle {cycle}: original and sidecar are preserved"
            );
            assert_eq!(
                checkpoint.as_deref(),
                Some("after"),
                "cycle {cycle}: the certified family advances the checkpoint"
            );
            assert_eq!(
                result.stats.sync_token_blocked_reason, None,
                "cycle {cycle}: {result:?}"
            );
        }
    }

    match ambiguity {
        RetainedAmbiguity::FutureDeferred => {
            assert_eq!(
                retry_row(&database),
                retry_before,
                "the deferred deadline and its single attempt are retained"
            );
            let (attempts, last_attempt_at, next_retry_at): (i64, i64, i64) =
                rusqlite::Connection::open(&database)
                    .unwrap()
                    .query_row(
                        "SELECT attempts,last_attempt_at,next_retry_at FROM metadata_capture_retries WHERE asset_id='legacy-master'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap();
            assert_eq!(attempts, 1, "one deferred ambiguity attempt");
            assert!(
                next_retry_at > last_attempt_at,
                "the deferred capture deadline stays in the future"
            );
            let (revision, last_error): (i64, Option<String>) =
                rusqlite::Connection::open(&database)
                    .unwrap()
                    .query_row(
                        "SELECT r.revision,s.last_error FROM asset_metadata_capture_revisions r JOIN metadata_capture_state s ON s.library=r.library WHERE r.asset_id='legacy-master' AND r.library='PrimarySync'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
            assert_eq!(revision, 0, "deferred capture must not complete identity");
            assert_eq!(last_error, None, "deferral is not a refusal attempt");
        }
        RetainedAmbiguity::AlreadyDue => {
            let (attempts, last_attempt_at, next_retry_at): (i64, i64, i64) =
                rusqlite::Connection::open(&database)
                    .unwrap()
                    .query_row(
                        "SELECT attempts,last_attempt_at,next_retry_at FROM metadata_capture_retries WHERE asset_id='legacy-master'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap();
            assert_eq!(attempts, 2, "the due ambiguity is a genuine retry");
            assert!(
                next_retry_at > last_attempt_at,
                "the refused identity retains backoff rather than resetting its deadline"
            );
            let (revision, last_error): (i64, Option<String>) =
                rusqlite::Connection::open(&database)
                    .unwrap()
                    .query_row(
                        "SELECT r.revision,s.last_error FROM asset_metadata_capture_revisions r JOIN metadata_capture_state s ON s.library=r.library WHERE r.asset_id='legacy-master' AND r.library='PrimarySync'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
            assert_eq!(
                revision, 0,
                "the refused legacy identity must not invent its completed revision"
            );
            assert_eq!(
                last_error.as_deref(),
                Some("multiple provider children matched durable catalogue evidence"),
                "exact refusal owner"
            );
        }
    }

    assert!(
        inventory.load(Ordering::SeqCst) > 0,
        "certification reads the complete provider inventory"
    );

    // Quiet reopen: an empty after-cursor delta keeps the active proof, the
    // refusal evidence, the owners and the advanced checkpoint unchanged.
    let (inner, result) = run_cycle(cycle_count).await;
    assert_eq!(result.stats.downloaded, 0, "{result:?}");
    assert_eq!(result.stats.sync_token_blocked_reason, None, "{result:?}");
    assert_eq!(std::fs::read(&original).unwrap(), bytes);
    assert_eq!(
        std::fs::read(&sidecar).unwrap(),
        b"legacy sidecar: never replace"
    );
    assert_eq!(historical_rows(&database), historical);
    assert_eq!(owner_rows(&database), owners_before);
    let presences = inner.legacy_preservations("PrimarySync").await.unwrap();
    assert_eq!(
        presences.len(),
        1,
        "the family proof survives the quiet reopen"
    );
    assert_eq!(presences[0].asset_id, "legacy-master");
    assert_eq!(presences[0].active_generation, Some(1));
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("after"),
        "the certified checkpoint stays advanced"
    );
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
