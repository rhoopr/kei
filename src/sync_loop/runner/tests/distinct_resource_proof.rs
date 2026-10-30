#![cfg(test)]

//! Synthetic cycle control for a changed distinct-occupant receipt.
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle, run_cycle_expected_date_dir,
};
use crate::{download, state};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};
const SHARED: &str = "SharedSync-SYNTHETIC-KEYS";
const SHARED_TOKEN: &str = "sync_token:SharedSync-SYNTHETIC-KEYS";

#[derive(Clone, Debug)]
struct Provider {
    zone: String,
    records: Vec<Value>,
}

#[async_trait::async_trait]
impl PhotosSession for Provider {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index-state","recordType":"CheckIndexingState",
                "fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/zones/list") {
            return Ok(json!({"zones":[{
                "zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
                "syncToken":"shared-before-inventory"
            }]}));
        }
        if url.contains("/records/query/batch") {
            return Ok(album_count_response(
                self.records
                    .iter()
                    .filter(|record| record["recordType"] == "CPLAsset")
                    .count() as u64,
            ));
        }
        if url.contains("/records/query?") {
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records.clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"shared-inventory"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|item| item["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        assert!(
            url.contains("/changes/zone?"),
            "unexpected synthetic provider endpoint: {url}"
        );
        let delta = request["zones"][0]["syncToken"].is_string();
        let records = if delta {
            Vec::new()
        } else {
            self.records.clone()
        };
        Ok(json!({"zones":[{
            "zoneID":{"zoneName":self.zone,"ownerRecordName":"_defaultOwner"},
            "syncToken":if delta {"shared-delta-after"} else {"shared-inventory-after"},
            "moreComing":false,
            "records":records
        }]}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn page_records(
    zone: &str,
    master: &str,
    child: &str,
    filename: &str,
    url: &str,
    bytes: &[u8],
    checksum: &str,
) -> Vec<Value> {
    let page =
        full_album_page_with_download(zone, master, "inventory", url, bytes.len() as u64, checksum);
    let mut master_record = page["records"][0].clone();
    master_record["recordName"] = json!(master);
    master_record["fields"]["filenameEnc"] = json!({
        "value": filename,
        "type": "STRING",
    });
    let mut child_record = page["records"][1].clone();
    child_record["recordName"] = json!(child);
    child_record["fields"]["masterRef"]["value"]["recordName"] = json!(master);
    child_record["fields"]["masterRef"]["value"]["zoneID"]["zoneName"] = json!(zone);
    vec![master_record, child_record]
}

#[tokio::test]
async fn distinct_receipt_changed_during_file_check_holds_second_resource() {
    use crate::commands::{AlbumPass, PassKind};
    let server = wiremock::MockServer::start().await;
    let first = include_bytes!("../../../../tests/data/media/pattern.jpg").as_slice();
    let second = include_bytes!("../../../../tests/data/media/metadata.jpg").as_slice();
    let checksums = [
        "ATLF2mUdMsryhdyIJgwrpiBlt1z2",
        "AV/Hz7fW6Rc+X19s6b+avvkD/0Sf",
    ];
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(first))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/second"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(second))
        .expect(0)
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state.db");
    let media = directory.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let occupant = media
        .join(run_cycle_expected_date_dir())
        .join("collision.JPG");
    let destination = media
        .join(run_cycle_expected_date_dir())
        .join(format!("collision-{}.JPG", second.len()));
    let snapshot = Arc::new(Mutex::new(None));
    let captured = snapshot.clone();
    let occupant_for_hook = occupant.clone();
    let database_for_hook = database.clone();
    let recheck_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_recheck = recheck_failed.clone();
    crate::download::planner::distinct_occupant_test_hook::install(
        occupant.clone(),
        Box::new(move || {
            let before = (
                std::fs::read(&occupant_for_hook).unwrap(),
                std::fs::metadata(&occupant_for_hook)
                    .unwrap()
                    .modified()
                    .unwrap(),
            );
            let conn = rusqlite::Connection::open(database_for_hook).unwrap();
            assert_eq!(conn.execute("UPDATE download_receipts SET proof_hash='changed-during-check' WHERE child='asset-distinct-a'", []).unwrap(), 1);
            *captured.lock().unwrap() = Some(before);
        }),
        Box::new(move |failed, exact| {
            assert!(!exact);
            observed_recheck.store(failed, std::sync::atomic::Ordering::SeqCst);
        }),
    );
    let mut records = page_records(
        SHARED,
        "distinct-master-a",
        "asset-distinct-a",
        "collision.jpg",
        &format!("{}/first", server.uri()),
        first,
        checksums[0],
    );
    records.extend(page_records(
        SHARED,
        "distinct-master-b",
        "asset-distinct-b",
        "collision.jpg",
        &format!("{}/second", server.uri()),
        second,
        checksums[1],
    ));
    let owner = state::db::account::AccountOwner::authenticated(
        "distinct@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-distinct"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&database, &owner)
            .await
            .unwrap(),
    );
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".into(),
        Arc::new(std::collections::HashMap::new()),
        Box::new(Provider {
            zone: SHARED.into(),
            records,
        }),
        Arc::new(json!({"zoneName":SHARED,"ownerRecordName":"_defaultOwner"})),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
        db.clone(),
        owner,
        "com",
    ));
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
    }];
    let mut selected = make_run_cycle_library_state_with_passes(SHARED, SHARED_TOKEN, passes);
    selected.library = library;
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let error = crate::sync_cycle::run_cycle(
        &[&selected],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &make_run_cycle_download_config_builder(&media, db.clone()),
        download::DownloadControls::download_hidden(),
        &session,
        &CancellationToken::new(),
    )
    .await
    .expect_err("changed receipt must refuse the selected cycle");
    assert!(
        matches!(
            error.downcast_ref::<crate::state::error::StateError>(),
            Some(crate::state::error::StateError::ProviderSelectionInvalid)
        ),
        "{error:?}"
    );
    let before = snapshot
        .lock()
        .unwrap()
        .clone()
        .expect("receipt was changed after confined byte validation");
    assert!(
        recheck_failed.load(std::sync::atomic::Ordering::SeqCst),
        "fresh receipt recheck must reject the changed proof"
    );
    assert_eq!(before.0, first);
    assert_eq!(std::fs::read(&occupant).unwrap(), before.0);
    assert_eq!(
        std::fs::metadata(&occupant).unwrap().modified().unwrap(),
        before.1
    );
    assert!(!destination.exists());
    {
        let conn = db.acquire_lock("changed receipt remains held").unwrap();
        let proof: String = conn
            .query_row(
                "SELECT proof_hash FROM download_receipts WHERE child='asset-distinct-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(proof, "changed-during-check");
        let second_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM download_receipts WHERE child='asset-distinct-b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(second_count, 0);
    }
    server.verify().await;
}

#[tokio::test]
async fn distinct_collision_pending_destination_respects_changed_filename_policy() {
    use crate::commands::{AlbumPass, PassKind};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let server = wiremock::MockServer::start().await;
    let first = include_bytes!("../../../../tests/data/media/pattern.jpg").as_slice();
    let second = include_bytes!("../../../../tests/data/media/metadata.jpg").as_slice();
    let checksums = [
        "ATLF2mUdMsryhdyIJgwrpiBlt1z2",
        "AV/Hz7fW6Rc+X19s6b+avvkD/0Sf",
    ];
    Mock::given(method("GET"))
        .and(path("/policy-first"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(first))
        .expect(1)
        .mount(&server)
        .await;
    let second_gets = Arc::new(AtomicUsize::new(0));
    let counter = second_gets.clone();
    Mock::given(method("GET"))
        .and(path("/policy-second"))
        .respond_with(move |_request: &wiremock::Request| {
            counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(500)
        })
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("state.db");
    let media = directory.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let occupant = media
        .join(run_cycle_expected_date_dir())
        .join("collision.JPG");
    let destination = media
        .join(run_cycle_expected_date_dir())
        .join(format!("collision-{}.JPG", second.len()));
    let matched = Arc::new(AtomicBool::new(false));
    let observed = matched.clone();
    let policy_match_attempted = Arc::new(AtomicBool::new(false));
    crate::download::planner::distinct_occupant_test_hook::install(
        occupant.clone(),
        Box::new(|| {}),
        Box::new(move |failed, exact| {
            assert!(!failed && exact);
            observed.store(true, Ordering::SeqCst);
        }),
    );
    let mut records = page_records(
        SHARED,
        "distinct-master-a",
        "asset-distinct-a",
        "collision.jpg",
        &format!("{}/policy-first", server.uri()),
        first,
        checksums[0],
    );
    records.extend(page_records(
        SHARED,
        "distinct-master-b",
        "asset-distinct-b",
        "collision.jpg",
        &format!("{}/policy-second", server.uri()),
        second,
        checksums[1],
    ));
    let owner = state::db::account::AccountOwner::authenticated(
        "distinct@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-distinct"}})).unwrap(),
    )
    .unwrap();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut baseline = None;
    let mut frozen_pending = None;
    for excluded in [false, true] {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&database, &owner)
                .await
                .unwrap(),
        );
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::new(std::collections::HashMap::new()),
            Box::new(Provider {
                zone: SHARED.into(),
                records: records.clone(),
            }),
            Arc::new(json!({"zoneName":SHARED,"ownerRecordName":"_defaultOwner"})),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "com",
        ));
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }];
        let mut selected = make_run_cycle_library_state_with_passes(SHARED, SHARED_TOKEN, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&media, db.clone());
        let builder = |mode, ids, groupings, zone| {
            let mut config = base(mode, ids, groupings, zone);
            let config_mut = Arc::make_mut(&mut config);
            config_mut.retry.max_retries = 0;
            if excluded {
                config_mut.filename_exclude = Arc::from([glob::Pattern::new("*.JPG").unwrap()]);
                config_mut.enum_config_hash = Some("changed-exclusion-policy".into());
            }
            config
        };
        let before_gets = second_gets.load(Ordering::SeqCst);
        let result = crate::sync_cycle::run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let bytes = std::fs::read(&occupant).unwrap();
        assert_eq!(bytes, first);
        let mtime = std::fs::metadata(&occupant).unwrap().modified().unwrap();
        type PendingRow = (String, String, String, String, i64);
        let (receipt, pending, second_receipts): (String, Vec<PendingRow>, i64) = {
            let conn = db.acquire_lock("distinct policy proof").unwrap();
            let receipt: String = conn.query_row(
                "SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-distinct-a'", [], |r| r.get(0)).unwrap();
            let pending: Vec<PendingRow> = conn.prepare(
                "SELECT generation,pass_key,path,checksum,size_bytes FROM provider_active_destinations WHERE child='asset-distinct-b' AND admitted=1 AND verified_media=0 ORDER BY generation,pass_key,path"
            ).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .unwrap().collect::<Result<_, _>>().unwrap();
            let second_receipts: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM download_receipts WHERE child='asset-distinct-b'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            (receipt, pending, second_receipts)
        };
        assert!(!destination.exists());
        assert_eq!(second_receipts, 0);
        assert!(
            !pending.is_empty(),
            "second admitted obligation must remain pending: {result:?}"
        );
        assert!(
            pending.iter().any(|entry| {
                serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                    &entry.2,
                )
                .is_ok_and(|path| path.to_path() == destination)
                    && entry.3 == checksums[1]
                    && entry.4 == second.len() as i64
            }),
            "the original second destination and resource must remain admitted"
        );
        if excluded {
            assert_eq!(
                second_gets.load(Ordering::SeqCst),
                before_gets,
                "changed exclusion must prevent another second-resource GET"
            );
            assert_eq!(result.stats.downloaded, 0, "{result:?}");
            assert_eq!((bytes, mtime, receipt), baseline.take().unwrap());
            assert!(
                pending
                    .iter()
                    .any(|entry| Some(entry) == frozen_pending.as_ref()),
                "the original admitted destination must remain pending"
            );
            assert!(
                !policy_match_attempted.load(Ordering::SeqCst),
                "excluded work must not invoke the distinct-occupant exception"
            );
            crate::download::planner::distinct_occupant_test_hook::discard(&occupant);
        } else {
            assert!(
                matched.load(Ordering::SeqCst),
                "the distinct-occupant proof and successful reload must run"
            );
            assert!(
                second_gets.load(Ordering::SeqCst) > 0,
                "the independently admitted second resource was attempted"
            );
            frozen_pending = pending.first().cloned();
            baseline = Some((bytes, mtime, receipt));
            let observed_policy_match = policy_match_attempted.clone();
            crate::download::planner::distinct_occupant_test_hook::install(
                occupant.clone(),
                Box::new(move || observed_policy_match.store(true, Ordering::SeqCst)),
                Box::new(|_, _| {}),
            );
            db.make_selection_retry_due(owner.clone(), "asset-distinct-b".into())
                .await
                .unwrap();
        }
    }
    server.verify().await;
}
