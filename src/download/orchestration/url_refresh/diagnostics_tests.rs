//! Disposable synthetic qualification. All identities and resources are invented.
use super::super::test_support::{incremental_photo_records_with_url, retry_test_task};
use super::{RetryTaskKey, UrlRetrySource, refresh_failed_download_urls};
use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::{PhotosService, PhotosSession, classify_legacy_inventory_error};
use crate::retry::RetryConfig;
use crate::state::{SqliteStateDb, VersionSizeKey};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tracing::instrument::WithSubscriber;

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_poisoned| std::io::Error::other("test log lock poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
async fn capture<T>(future: impl std::future::Future<Output = T>) -> (T, String) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let sink = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_target(false)
        .with_writer(move || LogWriter(sink.clone()))
        .finish();
    let result = future.with_subscriber(subscriber).await;
    let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    (result, log)
}
fn assert_redacted(log: &str, private_path: &std::path::Path) {
    for private in [
        "PRIVATE_",
        "asset-PRIVATE_",
        "synthetic-fresh",
        "synthetic-healthy",
        "https://",
        "token=",
        private_path.to_str().unwrap(),
    ] {
        assert!(
            !log.contains(private),
            "diagnostic disclosed fixture-private data"
        );
    }
}

#[derive(Clone, Debug)]
struct Fixture {
    fail_lookup: bool,
    discover_owner: bool,
    records: Arc<Vec<Value>>,
    pages: Arc<Vec<Value>>,
    calls: Arc<Mutex<Vec<Value>>>,
    listings: Arc<Mutex<usize>>,
}
#[async_trait::async_trait]
impl PhotosSession for Fixture {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        self.calls.lock().unwrap().push(request.clone());
        if request.pointer("/query/recordType").and_then(Value::as_str)
            == Some("CheckIndexingState")
        {
            return Ok(json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]}));
        }
        if url.contains("/zones/list") {
            return Ok(if url.contains("/shared/") {
                json!({"zones":[]})
            } else {
                self.zone_list()
            });
        }
        if url.contains("/changes/zone?") {
            let index = usize::from(request.pointer("/zones/0/syncToken").is_some());
            return Ok(self.pages[index].clone());
        }
        assert!(
            url.contains("/records/lookup?"),
            "no surrounding enumeration permitted"
        );
        if self.fail_lookup {
            return Err(crate::icloud::photos::session::HttpStatusError {
                status: 401,
                url: "https://PRIVATE_URL_CANARY/?token=PRIVATE_TOKEN_CANARY".into(),
                retry_after: None,
                body: Some("PRIVATE_PROVIDER_PAYLOAD_CANARY".into()),
            }
            .into());
        }
        let names: FxHashSet<_> = request
            .get("records")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|r| r.get("recordName").and_then(Value::as_str).unwrap())
            .collect();
        assert!(names.len() <= 4);
        Ok(
            json!({"records":self.records.iter().filter(|r| names.contains(r.get("recordName").and_then(Value::as_str).unwrap())).cloned().collect::<Vec<_>>()}),
        )
    }
    async fn post_changes_body(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        assert!(url.ends_with("/zones/list"));
        *self.listings.lock().unwrap() += 1;
        Ok(serde_json::to_vec(&if url.contains("/shared/") {
            json!({"zones":[]})
        } else {
            self.zone_list()
        })?)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
impl Fixture {
    fn zone_list(&self) -> Value {
        if self.discover_owner {
            json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}]})
        } else {
            json!({"zones":[]})
        }
    }
    fn new(discover_owner: bool, records: Vec<Value>, pages: Vec<Value>) -> Self {
        Self {
            fail_lookup: false,
            discover_owner,
            records: Arc::new(records),
            pages: Arc::new(pages),
            calls: Arc::new(Mutex::new(Vec::new())),
            listings: Arc::new(Mutex::new(0)),
        }
    }
    async fn actual_album(&self, route: &str) -> crate::icloud::photos::PhotoAlbum {
        let mut service = PhotosService::new(
            "https://synthetic.invalid".into(),
            Box::new(self.clone()),
            Default::default(),
            RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            },
        )
        .await
        .unwrap();
        if route == "all" {
            service
                .all_libraries()
                .await
                .unwrap()
                .into_iter()
                .find(|library| library.zone_name() == "PrimarySync")
                .unwrap()
                .all()
        } else {
            service.get_library("PrimarySync").await.unwrap().all()
        }
    }
}

#[tokio::test]
async fn exact_refresh_diagnostics_pair_and_record_scope_rejections() {
    use crate::icloud::photos::{ProviderRecordId, RecordLookupRequest, RecordResolution};
    for fault in ["pair", "record_scope", "component"] {
        let mut records = family();
        let (reason, stage) = match fault {
            "pair" => {
                records[1]["fields"]["masterRef"]["value"]["recordName"] =
                    json!("PRIVATE_WRONG_MASTER_CANARY");
                ("child_master_mismatch", "pairing")
            }
            "record_scope" => {
                records[0]["zoneID"] = json!({"zoneName":"PRIVATE_ZONE_CANARY"});
                ("zone_conflict", "record_scope")
            }
            _ => {
                records[0]["zoneID"] =
                    json!({"zoneName":"PrimarySync","PRIVATE_FIELD_CANARY":"PRIVATE_VALUE_CANARY"});
                ("scope_component_conflict", "record_scope")
            }
        };
        let fixture = Fixture::new(true, records, vec![]);
        let album = fixture.actual_album("primary").await;
        let requests = [RecordLookupRequest::paired(
            ProviderRecordId::new("asset-PRIVATE_MASTER_ID_CANARY"),
            ProviderRecordId::new("PRIVATE_MASTER_ID_CANARY"),
            ProviderRecordId::new("asset-PRIVATE_MASTER_ID_CANARY"),
        )];
        let (batch, log) = capture(album.resolve_records(&requests)).await;
        assert!(matches!(batch.results[0].1, RecordResolution::Unknown));
        assert!(log.contains("exact_lookup_rejection_v1"));
        assert!(log.contains(&format!("reason=\"{reason}\"")));
        assert!(log.contains(&format!("stage=\"{stage}\"")));
        assert!(log.contains("target=\"paired\""));
        assert!(log.contains("rejected_requests=1"));
        assert_redacted(&log, std::path::Path::new("/PRIVATE_PATH_CANARY"));
    }
}

#[tokio::test]
async fn exact_refresh_diagnostics_cancel_and_auth_keep_task_counts() {
    for fault in ["cancel", "auth"] {
        let mut fixture = Fixture::new(true, family(), vec![]);
        fixture.fail_lookup = fault == "auth";
        let passes = [AlbumPass {
            kind: PassKind::Album,
            album: fixture.actual_album("primary").await,
            exclude_ids: Arc::new(FxHashSet::default()),
        }];
        let tasks = [retry_test_task(
            "asset-PRIVATE_MASTER_ID_CANARY",
            VersionSizeKey::Original,
            "PRIVATE_PATH_CANARY",
        )];
        let cancel = CancellationToken::new();
        if fault == "cancel" {
            cancel.cancel();
        }
        let (plan, log) =
            capture(refresh_failed_download_urls(&passes, &tasks, &cancel, None)).await;
        assert!(plan.tasks.is_empty());
        assert_eq!(plan.unrefreshed.len(), 1);
        let summary = log
            .lines()
            .find(|line| line.contains("exact_refresh_outcomes_v1"))
            .unwrap();
        assert!(summary.contains("requested_task_keys=1"));
        assert!(summary.contains("remaining_task_keys=1"));
        assert!(summary.contains("refreshed_task_keys=0"));
        assert!(summary.contains("planned_paired_requests=0"));
        assert!(summary.contains("paired_lookup_results=0"));
        assert!(summary.contains("present_pairs=0"));
        assert!(summary.contains("child_master_references=0"));
        assert!(summary.contains(&format!(
            "child_lookup_results={}",
            usize::from(fault == "auth")
        )));
        assert!(summary.contains(&format!(
            "planned_unique_child_requests={}",
            usize::from(fault == "auth")
        )));
        assert!(summary.contains(&format!("authentication_failed={}", fault == "auth")));
        assert!(summary.contains(&format!("cancellation_observed={}", fault == "cancel")));
        assert_redacted(&log, std::path::Path::new("/PRIVATE_PATH_CANARY"));
    }
}
fn family() -> Vec<Value> {
    let mut records = incremental_photo_records_with_url(
        "PRIVATE_MASTER_ID_CANARY",
        "family.jpg",
        "https://p01.icloud-content.com/synthetic-fresh",
        1024,
    );
    records[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":"https://p01.icloud-content.com/synthetic-fresh-live","size":2048,"fileChecksum":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}});
    records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    records
}

#[tokio::test]
async fn exact_refresh_diagnostics_scope_and_resource_matrix() {
    for discover_owner in [false, true] {
        for route in ["primary", "all"] {
            for fault in [
                "bare",
                "same_owner",
                "foreign_owner",
                "foreign_zone",
                "partial",
                "missing_child",
                "missing_master",
                "checksum",
                "size",
                "rendition",
                "master_name",
            ] {
                let mut records = family();
                match fault {
                    "same_owner" => {
                        records[1]["fields"]["masterRef"]["value"]["zoneID"] =
                            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})
                    }
                    "foreign_owner" => {
                        records[1]["fields"]["masterRef"]["value"]["zoneID"] = json!({"zoneName":"PrimarySync","ownerRecordName":"PRIVATE_SCOPE_CANARY"})
                    }
                    "foreign_zone" => {
                        records[1]["fields"]["masterRef"]["value"]["zoneID"] = json!({"zoneName":"SharedSync-PRIVATE_SCOPE_CANARY","ownerRecordName":"_defaultOwner"})
                    }
                    "partial" => {
                        records[1]["fields"]["masterRef"]["value"]["zoneID"] =
                            json!({"ownerRecordName":"_defaultOwner"})
                    }
                    "missing_child" => {
                        records.remove(1);
                    }
                    "missing_master" => {
                        records.remove(0);
                    }
                    "checksum" => {
                        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                            json!("changed")
                    }
                    "size" => records[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(1025),
                    "rendition" => {
                        records[0]["fields"]
                            .as_object_mut()
                            .unwrap()
                            .remove("resOriginalRes");
                    }
                    "master_name" => {
                        records[1]["fields"]["masterRef"]["value"]["recordName"] =
                            json!("PRIVATE_CHANGED_MASTER_CANARY")
                    }
                    _ => {}
                }
                records.extend(incremental_photo_records_with_url(
                    "PRIVATE_HEALTHY_ID_CANARY",
                    "healthy.jpg",
                    "https://p01.icloud-content.com/synthetic-healthy",
                    1024,
                ));
                let fixture = Fixture::new(discover_owner, records, vec![]);
                let pass = AlbumPass {
                    kind: PassKind::Album,
                    album: fixture.actual_album(route).await,
                    exclude_ids: Arc::new(FxHashSet::default()),
                };
                let dir = TempDir::new().unwrap();
                std::fs::write(dir.path().join("sentinel"), b"existing synthetic media").unwrap();
                let mut tasks = Vec::new();
                let mut sources = FxHashMap::default();
                for (child, master, rendition, size) in [
                    (
                        "asset-PRIVATE_MASTER_ID_CANARY",
                        "PRIVATE_MASTER_ID_CANARY",
                        VersionSizeKey::Original,
                        1024,
                    ),
                    (
                        "asset-PRIVATE_MASTER_ID_CANARY",
                        "PRIVATE_MASTER_ID_CANARY",
                        VersionSizeKey::LiveOriginal,
                        2048,
                    ),
                    (
                        "asset-PRIVATE_HEALTHY_ID_CANARY",
                        "PRIVATE_HEALTHY_ID_CANARY",
                        VersionSizeKey::Original,
                        1024,
                    ),
                ] {
                    let mut task = retry_test_task(child, rendition, "unused");
                    task.download_path = dir.path().join(format!("{child}-{rendition:?}"));
                    task.size = size;
                    sources.insert(
                        RetryTaskKey::from(&task),
                        UrlRetrySource {
                            asset_record_name: Arc::from(child),
                            pass_index: 0,
                            master_record_name: Arc::from(master),
                            provider_version: rendition,
                        },
                    );
                    tasks.push(task);
                }
                if fault == "checksum" {
                    tasks.push(tasks[0].clone());
                }
                let (plan, log) = capture(refresh_failed_download_urls(
                    &[pass],
                    &tasks,
                    &CancellationToken::new(),
                    Some(&sources),
                ))
                .await;
                let expected_family = match fault {
                    "bare" => 2,
                    "same_owner" if discover_owner => 2,
                    "checksum" | "size" | "rendition" => 1,
                    _ => 0,
                };
                assert_eq!(
                    plan.tasks.len(),
                    1 + expected_family,
                    "{discover_owner} {fault}"
                );
                assert_eq!(
                    plan.unrefreshed
                        .iter()
                        .map(RetryTaskKey::from)
                        .collect::<FxHashSet<_>>()
                        .len(),
                    2 - expected_family,
                    "{discover_owner} {fault}"
                );
                assert_eq!(plan.provider_auth_errors, 0);
                for task in &plan.tasks {
                    let original = tasks
                        .iter()
                        .find(|t| RetryTaskKey::from(*t) == RetryTaskKey::from(task))
                        .unwrap();
                    assert_eq!(task.checksum, original.checksum);
                    assert_eq!(task.size, original.size);
                    assert_eq!(task.download_path, original.download_path);
                }
                assert_redacted(&log, dir.path());
                let expected_reason = match fault {
                    "same_owner" if !discover_owner => Some("owner_unqualified"),
                    "foreign_owner" => Some(if discover_owner {
                        "owner_conflict"
                    } else {
                        "owner_unqualified"
                    }),
                    "foreign_zone" => Some("zone_conflict"),
                    "partial" => Some("partial_scope"),
                    "missing_child" => Some("child_record_omitted"),
                    "missing_master" => Some("master_record_omitted"),
                    "checksum" => Some("checksum_mismatch"),
                    "size" => Some("size_mismatch"),
                    "rendition" => Some("rendition_missing"),
                    "master_name" => Some("selected_master_mismatch"),
                    _ => None,
                };
                if let Some(reason) = expected_reason {
                    assert!(
                        log.lines().any(|line| line.contains("_rejection_v1")
                            && line.contains(&format!("reason=\"{reason}\""))),
                        "missing expected fixed rejection class"
                    );
                } else {
                    assert!(
                        !log.contains("_rejection_v1"),
                        "accepted matching subset was labeled rejected"
                    );
                }
                let summary = log
                    .lines()
                    .find(|line| line.contains("exact_refresh_outcomes_v1"))
                    .unwrap();
                assert!(summary.contains("requested_task_keys=3"));
                assert!(summary.contains("planned_unique_child_requests=2"));
                assert!(summary.contains("child_lookup_results=2"));
                let expected_child_references = match fault {
                    "bare" | "missing_master" | "checksum" | "size" | "rendition"
                    | "master_name" => 2,
                    "same_owner" if discover_owner => 2,
                    _ => 1,
                };
                let expected_paired =
                    expected_child_references - usize::from(fault == "master_name");
                let expected_present_pairs =
                    expected_paired - usize::from(fault == "missing_master");
                assert!(summary.contains(&format!(
                    "child_master_references={expected_child_references}"
                )));
                assert!(summary.contains(&format!("planned_paired_requests={expected_paired}")));
                assert!(summary.contains(&format!("paired_lookup_results={expected_paired}")));
                assert!(summary.contains(&format!("present_pairs={expected_present_pairs}")));
                assert!(summary.contains(&format!("refreshed_task_keys={}", 1 + expected_family)));
                assert!(summary.contains(&format!("remaining_task_keys={}", 2 - expected_family)));
                if matches!(fault, "checksum" | "size" | "rendition") {
                    let rejection = log
                        .lines()
                        .find(|line| line.contains("exact_refresh_task_rejection_v1"))
                        .unwrap();
                    // Two identical checksum inputs still represent one rejected task key.
                    assert!(rejection.contains("rejected_task_keys=1"));
                }
                let calls = fixture.calls.lock().unwrap();
                let lookups: Vec<_> = calls
                    .iter()
                    .filter(|r| r.get("records").is_some())
                    .collect();
                assert_eq!(lookups.len(), 2);
                assert_eq!(
                    lookups[0]["records"].as_array().unwrap().len(),
                    2,
                    "3 task keys collapse to 2 child lookups"
                );
                assert_eq!(
                    lookups[0]["zoneID"].get("ownerRecordName").is_some(),
                    discover_owner
                );
                assert_eq!(
                    std::fs::read(dir.path().join("sentinel")).unwrap(),
                    b"existing synthetic media"
                );
                assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
                eprintln!(
                    "route={route} scope={discover_owner} case={fault} requested_tasks=3 child_lookups=2 refreshed={} retained={} lookup_requests=2 writes=0",
                    plan.tasks.len(),
                    plan.unrefreshed.len()
                );
            }
        }
    }
}

fn page(records: Vec<Value>, cursor: &str, more: bool) -> Value {
    json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":records,"syncToken":cursor,"moreComing":more}]})
}
#[tokio::test]
async fn preservation_diagnostics_two_page_matrix() {
    for discover_owner in [false, true] {
        for fault in [
            "valid_unrelated",
            "sparse",
            "soft_deleted_sparse",
            "hard_deleted_sparse",
            "foreign_unrelated",
            "same_owner_family",
            "missing_capture_date",
            "repeat_cursor",
            "bad_envelope",
        ] {
            let mut first = family();
            if fault == "same_owner_family" {
                first[1]["fields"]["masterRef"]["value"]["zoneID"] =
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
            }
            if fault == "missing_capture_date" {
                first[1]["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("assetDate");
            }
            let mut sibling = first[1].clone();
            sibling["recordName"] = json!("PRIVATE_SECOND_CHILD_CANARY");
            let mut unrelated = json!({"recordName":"PRIVATE_UNRELATED_CHILD_CANARY","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"outside-family"}}}});
            match fault {
                "sparse" | "soft_deleted_sparse" | "hard_deleted_sparse" => {
                    unrelated["fields"]
                        .as_object_mut()
                        .unwrap()
                        .remove("masterRef");
                }
                "foreign_unrelated" => {
                    unrelated["fields"]["masterRef"]["value"]["zoneID"] =
                        json!({"zoneName":"PRIVATE_SCOPE_CANARY"})
                }
                _ => {}
            }
            if fault == "soft_deleted_sparse" {
                unrelated["fields"]["isDeleted"] = json!({"value":1});
            }
            if fault == "hard_deleted_sparse" {
                unrelated["deleted"] = json!(true);
            }
            let mut last = page(
                vec![sibling, unrelated],
                if fault == "repeat_cursor" {
                    "first"
                } else {
                    "final"
                },
                false,
            );
            if fault == "bad_envelope" {
                last["zones"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("moreComing");
            }
            let fixture = Fixture::new(
                discover_owner,
                vec![],
                vec![page(first, "first", true), last],
            );
            let album = fixture.actual_album("primary").await;
            let dir = TempDir::new().unwrap();
            let database = dir.path().join("state.db");
            std::fs::write(dir.path().join("sentinel"), b"synthetic legacy evidence").unwrap();
            let db = SqliteStateDb::open(&database).await.unwrap();
            db.set_metadata("sync_token:PrimarySync", "previous")
                .await
                .unwrap();
            db.set_metadata("unresolved_asset_identity:PrimarySync", "1")
                .await
                .unwrap();
            drop(db);
            let (result, log) = capture(async {
                let result = album
                    .complete_legacy_inventory(
                        &FxHashSet::from_iter(["PRIVATE_MASTER_ID_CANARY".to_string()]),
                        &CancellationToken::new(),
                    )
                    .await;
                if let Err(error) = &result {
                    crate::download::legacy_preservation::log_preservation_hold(
                        "preparation",
                        error,
                    );
                }
                result
            })
            .await;
            assert_redacted(&log, dir.path());
            let expected_success = matches!(fault, "valid_unrelated" | "hard_deleted_sparse")
                || (fault == "same_owner_family" && discover_owner);
            assert_eq!(result.is_ok(), expected_success, "{discover_owner} {fault}");
            if let Ok(inventory) = result {
                assert_eq!(inventory.children.len(), 2);
                assert_eq!(inventory.cursor, "final");
            } else if let Err(error) = result {
                let diagnostic = classify_legacy_inventory_error(&error).unwrap();
                assert_eq!(diagnostic.reason, "invalid_inventory_evidence");
                assert_eq!(diagnostic.pages, 2);
                assert!(diagnostic.transferred_bytes < 64 * 1024 * 1024);
                assert!(diagnostic.retained_bytes < 256 * 1024 * 1024);
                let (phase, subreason, family_context) = match fault {
                    "sparse" | "soft_deleted_sparse" => (
                        "child_reference",
                        "child_master_reference_missing",
                        "unknown",
                    ),
                    "foreign_unrelated" => (
                        "child_reference",
                        "child_reference_scope_mismatch",
                        "unrelated",
                    ),
                    "same_owner_family" => (
                        "child_reference",
                        "child_reference_scope_mismatch",
                        "candidate",
                    ),
                    "missing_capture_date" => (
                        "family_hydration",
                        "family_hydration_incomplete",
                        "candidate",
                    ),
                    "repeat_cursor" => ("cursor", "cursor_repeated", "not_applicable"),
                    "bad_envelope" => ("envelope", "completion_marker_missing", "not_applicable"),
                    _ => panic!("unexpected error"),
                };
                assert_eq!(diagnostic.phase, phase);
                assert_eq!(diagnostic.subreason, subreason);
                assert_eq!(diagnostic.family_context, family_context);
                assert_eq!(
                    diagnostic.child_soft_deleted,
                    fault == "soft_deleted_sparse"
                );
                assert_eq!(diagnostic.eof_observed, fault != "bad_envelope");
                assert!(log.contains("legacy_inventory_failure_v2"));
                assert!(log.contains(&format!("subreason=\"{subreason}\"")));
            }
            let reopened = SqliteStateDb::open(&database).await.unwrap();
            assert_eq!(
                reopened
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("previous")
            );
            assert_eq!(
                reopened
                    .get_metadata("unresolved_asset_identity:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("1")
            );
            assert_eq!(
                std::fs::read(dir.path().join("sentinel")).unwrap(),
                b"synthetic legacy evidence"
            );
            assert_eq!(
                fixture
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|r| r.get("zones").is_some())
                    .count(),
                2
            );
            eprintln!(
                "preservation scope={discover_owner} case={fault} pages=2 success={expected_success} checkpoint=retained debt=retained media_writes=0"
            );
        }
    }
}
