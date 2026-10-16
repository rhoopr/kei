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
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

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
    let (recorder, evidence) = crate::support::history::test_layer();
    let subscriber = tracing_subscriber::registry().with(recorder).with(
        tracing_subscriber::fmt::layer()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_writer(move || LogWriter(sink.clone()))
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
    );
    let result = future.with_subscriber(subscriber).await;
    for observation in evidence() {
        let fields = &observation["fields"];
        let text = observation.to_string();
        for private in [
            "PRIVATE_",
            "https://",
            "synthetic-fresh",
            "synthetic-healthy",
        ] {
            assert!(
                !text.contains(private),
                "support tracing projection leaked fixture data"
            );
        }
        if observation["kind"] == "exact_lookup_rejection_v1" {
            for key in [
                "target",
                "stage",
                "reason",
                "expected_owner",
                "lookup_zone",
                "rejected_requests",
                "supplied_owner",
                "supplied_owner_type",
                "owner_provenance",
            ] {
                assert!(
                    fields.get(key).is_some(),
                    "production lookup diagnostic lost {key}: {observation}"
                );
            }
        }
        if observation["kind"] == "legacy_inventory_failure_v2" {
            for key in [
                "stage",
                "reason",
                "phase",
                "subreason",
                "eof_observed",
                "family_context",
                "child_soft_deleted",
                "pages",
                "records",
                "transferred_bytes",
                "retained_bytes",
                "scope_mismatch_component",
                "expected_owner",
                "expected_owner_type",
                "supplied_owner",
                "supplied_owner_type",
                "elapsed_secs",
                "page_budget",
                "record_budget",
                "page_byte_budget",
                "retained_byte_budget",
            ] {
                assert!(
                    fields.get(key).is_some(),
                    "production inventory diagnostic lost {key}: {observation}"
                );
            }
        }
    }
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
    private_zones: Option<Value>,
    fail_indexing_zone: Option<&'static str>,
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
            let fail = request.pointer("/zoneID/ownerRecordName").is_some()
                && request.pointer("/zoneID/zoneName").and_then(Value::as_str)
                    == self.fail_indexing_zone;
            return Ok(
                json!({"records":[{"fields":{"state":{"value":if fail { "IN_PROGRESS" } else { "FINISHED" }}}}]}),
            );
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
        if let Some(zones) = &self.private_zones {
            return zones.clone();
        }
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
            private_zones: None,
            fail_indexing_zone: None,
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
        assert!(!batch.complete);
        assert!(matches!(
            batch.results[0].1,
            RecordResolution::Unknown | RecordResolution::MasterChanged { .. }
        ));
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
                "null_owner",
                "number_owner",
                "boolean_owner",
                "array_owner",
                "object_owner",
                "empty_owner",
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
                    "null_owner" | "number_owner" | "boolean_owner" | "array_owner"
                    | "object_owner" | "empty_owner" => {
                        let owner = match fault {
                            "null_owner" => Value::Null,
                            "number_owner" => json!(1),
                            "boolean_owner" => json!(true),
                            "array_owner" => json!([]),
                            "object_owner" => json!({}),
                            "empty_owner" => json!(""),
                            _ => panic!("unknown synthetic owner fault: {fault}"),
                        };
                        records[1]["fields"]["masterRef"]["value"]["zoneID"] =
                            json!({"zoneName":"PrimarySync","ownerRecordName":owner});
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
                    "foreign_owner" | "null_owner" | "number_owner" | "boolean_owner"
                    | "array_owner" | "object_owner" | "empty_owner" => Some(if discover_owner {
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
                if let Some((owner_class, owner_type)) = match fault {
                    "same_owner" if !discover_owner => Some(("private_default", "string")),
                    "foreign_owner" => Some(("other_string", "string")),
                    "null_owner" => Some(("null", "null")),
                    "number_owner" => Some(("malformed", "number")),
                    "boolean_owner" => Some(("malformed", "boolean")),
                    "array_owner" => Some(("malformed", "array")),
                    "object_owner" => Some(("malformed", "object")),
                    "empty_owner" => Some(("empty", "string")),
                    _ => None,
                } {
                    let rejection = log
                        .lines()
                        .find(|line| line.contains("exact_lookup_rejection_v1"))
                        .unwrap();
                    assert!(
                        rejection.contains(&format!("supplied_owner=\"{owner_class}\"")),
                        "{rejection}"
                    );
                    assert!(
                        rejection.contains(&format!("supplied_owner_type=\"{owner_type}\"")),
                        "{rejection}"
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
                if matches!(
                    fault,
                    "sparse" | "soft_deleted_sparse" | "missing_capture_date"
                ) {
                    assert_eq!(diagnostic.supplied_owner, "not_recorded");
                    assert_eq!(diagnostic.supplied_owner_type, "not_recorded");
                } else if fault == "foreign_unrelated" {
                    assert_eq!(diagnostic.supplied_owner, "absent");
                    assert_eq!(diagnostic.supplied_owner_type, "absent");
                }
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

/// Capture the production allowlisted projection without using saved history or
/// private state. This checks producer meaning, not just allowlist membership.
async fn support_capture<T>(future: impl std::future::Future<Output = T>) -> (T, Vec<Value>) {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("support.json");
    let (recorder, guard) = crate::support::history::start(path.clone(), json!({})).unwrap();
    let result = future
        .with_subscriber(
            tracing_subscriber::registry().with(recorder).with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
            ),
        )
        .await;
    guard.finish().await;
    let observations = crate::support::history::test_retained_events(&path);
    for observation in &observations {
        assert!(!observation.to_string().contains("PRIVATE_"));
        assert!(!observation.to_string().contains("https://"));
    }
    (result, observations)
}

#[tokio::test]
async fn inventory_owner_diagnostics_distinguish_default_foreign_and_malformed() {
    let mut projections = Vec::new();
    for discover_owner in [false, true] {
        for (label, owner, owner_class, owner_type) in [
            (
                "default",
                json!("_defaultOwner"),
                "private_default",
                "string",
            ),
            ("foreign", json!("PRIVATE_OTHER"), "other_string", "string"),
            ("null", Value::Null, "null", "null"),
            ("number", json!(1000), "malformed", "number"),
            ("boolean", json!(true), "malformed", "boolean"),
            ("array", json!([]), "malformed", "array"),
            ("object", json!({}), "malformed", "object"),
            ("empty", json!(""), "empty", "string"),
        ] {
            let first = family();
            let mut sibling = first[1].clone();
            sibling["recordName"] = json!("PRIVATE_SECOND_CHILD_CANARY");
            let unrelated = json!({"recordName":"PRIVATE_UNRELATED_CHILD_CANARY","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"outside-family","zoneID":{"zoneName":"PrimarySync","ownerRecordName":owner}}}}});
            let fixture = Fixture::new(
                discover_owner,
                vec![],
                vec![
                    page(first, "first", true),
                    page(vec![sibling, unrelated], "final", false),
                ],
            );
            let album = fixture.actual_album("all").await;
            let (result, observations) = support_capture(async {
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
            assert_eq!(
                result.is_ok(),
                discover_owner && label == "default",
                "{discover_owner} {label}"
            );
            if let Err(error) = result {
                let diagnostic = classify_legacy_inventory_error(&error).unwrap();
                assert_eq!(diagnostic.subreason, "child_reference_scope_mismatch");
                assert_eq!(diagnostic.scope_mismatch_component, "owner");
                assert_eq!(diagnostic.family_context, "unrelated");
                assert!(diagnostic.eof_observed);
                let mut fields = observations
                    .iter()
                    .find(|o| o["kind"] == "legacy_inventory_failure_v2")
                    .unwrap()["fields"]
                    .clone();
                let map = fields.as_object_mut().unwrap();
                assert_eq!(
                    map["expected_owner"],
                    if discover_owner {
                        "private_default"
                    } else {
                        "absent"
                    }
                );
                assert_eq!(
                    map["expected_owner_type"],
                    if discover_owner { "string" } else { "absent" }
                );
                assert_eq!(map["supplied_owner"], owner_class);
                assert_eq!(map["supplied_owner_type"], owner_type);
                // Duration is not provenance. Equal-length fixtures previously
                // collided even when page, record and byte counters were retained.
                map.remove("elapsed_secs");
                if !discover_owner {
                    projections.push((label, fields));
                }
            }
        }
    }
    let projection = |label| {
        &projections
            .iter()
            .find(|(name, _)| *name == label)
            .unwrap()
            .1
    };
    assert_ne!(projection("default"), projection("foreign"));
    assert_ne!(projection("null"), projection("number"));
}

#[tokio::test]
async fn discovery_diagnostics_identify_qualified_primary_scope() {
    let mut projections = Vec::new();
    for (zone_name, owner, deleted, eligibility, owner_class, owner_type, qualified_primary) in [
        (
            Some("PrimarySync"),
            Some(json!("_defaultOwner")),
            false,
            "eligible",
            "private_default",
            "string",
            true,
        ),
        (
            Some("SharedSync-PRIVATE_OTHER"),
            Some(json!("_defaultOwner")),
            false,
            "no_primary",
            "absent",
            "absent",
            false,
        ),
        (None, None, false, "no_primary", "absent", "absent", false),
        (
            Some("PrimarySync"),
            Some(json!("_defaultOwner")),
            true,
            "primary_deleted",
            "private_default",
            "string",
            false,
        ),
        (
            Some("PrimarySync"),
            None,
            false,
            "owner_unqualified",
            "absent",
            "absent",
            false,
        ),
        (
            Some("PrimarySync"),
            Some(json!("PRIVATE_OTHER")),
            false,
            "eligible",
            "other_string",
            "string",
            true,
        ),
        (
            Some("PrimarySync"),
            Some(Value::Null),
            false,
            "owner_unqualified",
            "null",
            "null",
            false,
        ),
        (
            Some("PrimarySync"),
            Some(json!(1000)),
            false,
            "owner_unqualified",
            "malformed",
            "number",
            false,
        ),
        (
            Some("PrimarySync"),
            Some(json!(true)),
            false,
            "owner_unqualified",
            "malformed",
            "boolean",
            false,
        ),
        (
            Some("PrimarySync"),
            Some(json!([])),
            false,
            "owner_unqualified",
            "malformed",
            "array",
            false,
        ),
        (
            Some("PrimarySync"),
            Some(json!({})),
            false,
            "owner_unqualified",
            "malformed",
            "object",
            false,
        ),
        (
            Some("PrimarySync"),
            Some(json!("")),
            false,
            "owner_unqualified",
            "empty",
            "string",
            false,
        ),
    ] {
        let mut fixture = Fixture::new(false, vec![], vec![]);
        let zones = if let Some(zone_name) = zone_name {
            let mut zone = json!({"zoneID":{"zoneName":zone_name}, "deleted":deleted});
            if let Some(owner) = owner {
                zone["zoneID"]["ownerRecordName"] = owner;
            }
            vec![zone]
        } else {
            vec![]
        };
        fixture.private_zones = Some(json!({"zones":zones}));
        let (primary_is_qualified, observations) = support_capture(async {
            let mut service = PhotosService::new(
                "https://synthetic.invalid".into(),
                Box::new(fixture.clone()),
                Default::default(),
                RetryConfig {
                    max_retries: 0,
                    base_delay_secs: 0,
                    max_delay_secs: 0,
                },
            )
            .await
            .unwrap();
            let album = service
                .all_libraries()
                .await
                .unwrap()
                .into_iter()
                .find(|library| library.zone_name() == "PrimarySync")
                .unwrap()
                .all();
            use crate::icloud::photos::{ProviderRecordId, RecordLookupRequest};
            let _batch = album
                .resolve_records(&[RecordLookupRequest::paired(
                    ProviderRecordId::new("PRIVATE_CHILD"),
                    ProviderRecordId::new("PRIVATE_MASTER"),
                    ProviderRecordId::new("PRIVATE_CHILD"),
                )])
                .await;
            fixture
                .calls
                .lock()
                .unwrap()
                .iter()
                .find(|request| request.get("records").is_some())
                .unwrap()["zoneID"]
                .get("ownerRecordName")
                .and_then(Value::as_str)
                .is_some_and(|owner| !owner.trim().is_empty())
        })
        .await;
        assert_eq!(primary_is_qualified, qualified_primary);
        let fields: Vec<_> = observations
            .into_iter()
            .filter(|o| o["kind"] == "support_discovery_v1")
            .map(|o| o["fields"].clone())
            .collect();
        let discovery = fields
            .iter()
            .find(|event| event["phase"] == "primary_discovery")
            .unwrap();
        assert_eq!(discovery["database"], "private");
        assert_eq!(discovery["zone_class"], "primary");
        assert_eq!(discovery["listing_validated"], true);
        assert_eq!(discovery["primary_eligibility"], eligibility);
        assert_eq!(discovery["owner"], owner_class);
        assert_eq!(discovery["owner_type"], owner_type);
        assert_eq!(
            fields
                .iter()
                .any(|event| event["phase"] == "primary_publication"),
            qualified_primary
        );
        if qualified_primary {
            let published = fields
                .iter()
                .find(|event| event["phase"] == "primary_publication")
                .unwrap();
            assert_eq!(published["owner"], owner_class);
            assert_eq!(published["owner_type"], owner_type);
        }
        if zone_name == Some("SharedSync-PRIVATE_OTHER") {
            let selected = fields
                .iter()
                .find(|event| event["selected_indexing"] == true)
                .unwrap();
            assert_eq!(selected["zone_class"], "other");
            assert_eq!(selected["owner"], "private_default");
        }
        projections.push(fields);
    }
    // These two production-path cases previously had identical retained events.
    assert_ne!(projections[0], projections[1]);
}

#[tokio::test]
async fn discovery_diagnostics_do_not_publish_after_failed_listing_or_indexing() {
    for fault in [
        "invalid_listing",
        "primary_indexing",
        "later_private_indexing",
    ] {
        let mut fixture = Fixture::new(false, vec![], vec![]);
        let mut zones =
            vec![json!({"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}})];
        match fault {
            "invalid_listing" => zones.push(zones[0].clone()),
            "primary_indexing" => fixture.fail_indexing_zone = Some("PrimarySync"),
            "later_private_indexing" => {
                zones.push(json!({"zoneID":{"zoneName":"SharedSync-PRIVATE_OTHER","ownerRecordName":"_defaultOwner"}}));
                fixture.fail_indexing_zone = Some("SharedSync-PRIVATE_OTHER");
            }
            _ => panic!("unknown discovery fixture"),
        }
        fixture.private_zones = Some(json!({"zones":zones}));
        let (result, observations) = support_capture(async {
            let mut service = PhotosService::new(
                "https://synthetic.invalid".into(),
                Box::new(fixture),
                Default::default(),
                RetryConfig {
                    max_retries: 0,
                    base_delay_secs: 0,
                    max_delay_secs: 0,
                },
            )
            .await
            .unwrap();
            service.all_libraries().await
        })
        .await;
        assert!(result.is_err());
        assert!(
            !observations
                .iter()
                .any(|event| event["fields"]["phase"] == "primary_publication")
        );
        assert_eq!(
            observations
                .iter()
                .any(|event| event["fields"]["listing_validated"] == true),
            fault != "invalid_listing"
        );
        if fault == "later_private_indexing" {
            // Primary initialization succeeded, but the complete private map
            // failed before publication. Generic indexing success is insufficient.
            assert!(
                observations
                    .iter()
                    .any(|event| event["fields"]["zone_class"] == "primary"
                        && event["fields"]["selected_indexing"] == true)
            );
        }
    }
}

#[tokio::test]
async fn inventory_envelope_owner_diagnostics_preserve_request_scope_guards() {
    for discover_owner in [false, true] {
        for (owner, owner_class, owner_type) in [
            (json!("_defaultOwner"), "private_default", "string"),
            (json!("PRIVATE_OTHER"), "other_string", "string"),
            (Value::Null, "null", "null"),
            (json!(1000), "malformed", "number"),
            (json!(true), "malformed", "boolean"),
            (json!([]), "malformed", "array"),
            (json!({}), "malformed", "object"),
            (json!(""), "empty", "string"),
        ] {
            let mut response = page(family(), "final", false);
            response["zones"][0]["zoneID"]["ownerRecordName"] = owner;
            let fixture = Fixture::new(discover_owner, vec![], vec![response]);
            let album = fixture.actual_album("primary").await;
            let (result, observations) = support_capture(async {
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
            // A supplied envelope owner does not grant an ownerless request
            // authority. Preserve the existing envelope and reference asymmetry.
            assert_eq!(
                result.is_ok(),
                !discover_owner || owner_class == "private_default"
            );
            if result.is_err() {
                let fields = &observations
                    .iter()
                    .find(|event| event["kind"] == "legacy_inventory_failure_v2")
                    .unwrap()["fields"];
                assert_eq!(fields["phase"], "scope");
                assert_eq!(fields["subreason"], "zone_scope_mismatch");
                assert_eq!(fields["scope_mismatch_component"], "owner");
                assert_eq!(fields["expected_owner"], "private_default");
                assert_eq!(fields["expected_owner_type"], "string");
                assert_eq!(fields["supplied_owner"], owner_class);
                assert_eq!(fields["supplied_owner_type"], owner_type);
            }
        }
    }
}

#[tokio::test]
async fn issue765_unrelated_private_zone_cannot_replace_primary_owner() {
    use crate::icloud::photos::{ProviderRecordId, RecordLookupRequest, RecordResolution};

    for route in ["primary", "all"] {
        for owner in ["PRIVATE_OWNER_CANARY", "_defaultOwner"] {
            let mut records = family();
            records[1]["fields"]["masterRef"]["value"]["zoneID"] =
                json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
            let mut fixture = Fixture::new(false, records, vec![]);
            fixture.private_zones = Some(json!({"zones":[
                {"zoneID":{"zoneName":"PrimarySync","ownerRecordName":owner}},
                {"zoneID":{"zoneName":"SharedSync-PRIVATE_OTHER","ownerRecordName":"_defaultOwner"}}
            ]}));
            let album = fixture.actual_album(route).await;
            let requests = [RecordLookupRequest::paired(
                ProviderRecordId::new("asset-PRIVATE_MASTER_ID_CANARY"),
                ProviderRecordId::new("PRIVATE_MASTER_ID_CANARY"),
                ProviderRecordId::new("asset-PRIVATE_MASTER_ID_CANARY"),
            )];
            let (batch, log) = capture(album.resolve_records(&requests)).await;
            assert_eq!(batch.results.len(), 1);
            if owner == "_defaultOwner" {
                assert!(matches!(batch.results[0].1, RecordResolution::Present(_)));
            } else {
                assert!(matches!(batch.results[0].1, RecordResolution::Unknown));
                assert!(log.contains("owner_conflict"));
            }
            let calls = fixture.calls.lock().unwrap();
            let request = calls.iter().find(|r| r.get("records").is_some()).unwrap();
            assert_eq!(request["zoneID"]["ownerRecordName"], owner);
            assert_eq!(batch.complete, owner == "_defaultOwner");
            assert_redacted(&log, std::path::Path::new("/PRIVATE_PATH_CANARY"));
        }
    }
}

#[tokio::test]
async fn explicit_primary_owner_lookup_and_inventory_validate_exact_scope_on_repeat() {
    use crate::icloud::photos::{ProviderRecordId, RecordLookupRequest, RecordResolution};
    for route in ["primary", "all"] {
        for foreign in [false, true] {
            let scope = json!({"zoneName":"PrimarySync","ownerRecordName":"PRIVATE_OWNER_CANARY","zoneType":"REGULAR_CUSTOM_ZONE"});
            let mut records = family();
            records[1]["fields"]["masterRef"]["value"]["zoneID"] = if foreign {
                json!({"zoneName":"PrimarySync","ownerRecordName":"PRIVATE_FOREIGN_CANARY","zoneType":"REGULAR_CUSTOM_ZONE"})
            } else {
                scope.clone()
            };
            let mut unrelated = records[1].clone();
            unrelated["recordName"] = json!("PRIVATE_UNRELATED_CHILD");
            unrelated["fields"]["masterRef"]["value"]["recordName"] =
                json!("PRIVATE_UNRELATED_MASTER");
            let mut inventory_records = records.clone();
            inventory_records.push(unrelated);
            let mut inventory_page = page(inventory_records, "PRIVATE_FINAL_CURSOR", false);
            inventory_page["zones"][0]["zoneID"] = scope.clone();
            let mut fixture = Fixture::new(false, records, vec![inventory_page]);
            fixture.private_zones = Some(json!({"zones":[{"zoneID":scope}]}));
            // These provider owners validate scope without granting checkpoint authority.
            for _ in 0..2 {
                let album = fixture.actual_album(route).await;
                let batch = album
                    .resolve_records(&[RecordLookupRequest::paired(
                        ProviderRecordId::new("asset-PRIVATE_MASTER_ID_CANARY"),
                        ProviderRecordId::new("PRIVATE_MASTER_ID_CANARY"),
                        ProviderRecordId::new("asset-PRIVATE_MASTER_ID_CANARY"),
                    )])
                    .await;
                assert_eq!(batch.complete, !foreign);
                assert_eq!(
                    matches!(batch.results[0].1, RecordResolution::Present(_)),
                    !foreign
                );
                let inventory = album
                    .complete_legacy_inventory(
                        &FxHashSet::from_iter(["PRIVATE_MASTER_ID_CANARY".to_owned()]),
                        &CancellationToken::new(),
                    )
                    .await;
                if foreign {
                    let diagnostic =
                        classify_legacy_inventory_error(&inventory.unwrap_err()).unwrap();
                    assert_eq!(diagnostic.subreason, "child_reference_scope_mismatch");
                    assert_eq!(diagnostic.scope_mismatch_component, "owner");
                    assert!(diagnostic.eof_observed);
                } else {
                    assert_eq!(inventory.unwrap().children.len(), 1);
                }
                let calls = fixture.calls.lock().unwrap();
                assert!(
                    calls
                        .iter()
                        .filter(|call| call.get("records").is_some())
                        .all(|call| call["zoneID"] == scope)
                );
            }
        }
    }
}
