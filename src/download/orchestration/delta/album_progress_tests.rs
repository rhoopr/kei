use super::incremental_photo_records;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
use crate::retry::RetryConfig;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const PREFIX_RECORDS: usize = 4282;
const PREFIX_PAGES: usize = PREFIX_RECORDS.div_ceil(200);

#[derive(Clone)]
struct SparseFixture {
    started: Instant,
    trace: Arc<Mutex<Vec<Value>>>,
    attempts: Arc<Mutex<HashMap<(String, usize), usize>>>,
    page_seconds: u64,
    late_owner: bool,
    retry_page: Option<usize>,
    persistent_error: bool,
    retry_delay_secs: u64,
    gate_phase: &'static str,
    gate: Option<Arc<tokio::sync::Notify>>,
}
impl SparseFixture {
    fn new(page_seconds: u64, late_owner: bool) -> Self {
        Self {
            started: Instant::now(),
            trace: Arc::default(),
            attempts: Arc::default(),
            page_seconds,
            late_owner,
            retry_page: None,
            persistent_error: false,
            retry_delay_secs: 2,
            gate_phase: "changes",
            gate: None,
        }
    }
    fn event(&self, mut event: Value) {
        event["at_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.trace.lock().unwrap().push(event);
    }
    fn album(&self, zone: &str) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.invalid"),
                name: Arc::from("Synthetic album"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName":zone})),
                retry_config: RetryConfig {
                    max_retries: 1,
                    base_delay_secs: 0,
                    max_delay_secs: self.retry_delay_secs,
                },
                container_id: Some(Arc::from("synthetic-album")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(self.clone()),
        )
    }
    fn pairs(zone: &Value) -> Vec<Value> {
        let mut records = incremental_photo_records("LATE_A");
        records.extend(incremental_photo_records("LATE_B"));
        for record in &mut records {
            record["zoneID"] = zone.clone();
            if record["recordType"] == "CPLAsset" {
                record["fields"]["masterRef"]["value"]["zoneID"] = zone.clone();
            }
        }
        records
    }
}
#[async_trait::async_trait]
impl PhotosSession for SparseFixture {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let body: Value = serde_json::from_str(&body)?;
        if url.contains("/records/lookup?") {
            let zone = &body["zoneID"];
            let source = zone["zoneName"].as_str().unwrap();
            let names: Vec<_> = body["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["recordName"].as_str().unwrap().to_owned())
                .collect();
            self.event(json!({"kind":"lookup_start","source":source,"records":names}));
            if let Some(gate) = &self.gate
                && (self.gate_phase == "asset_lookup"
                    || (self.gate_phase == "paired_lookup"
                        && names.iter().any(|n| !n.starts_with("asset-"))))
            {
                gate.notify_one();
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            let records: Vec<_> = if source == "PrimarySync" {
                names
                    .iter()
                    .map(|name| {
                        assert!(
                            name.starts_with("asset-"),
                            "sparse children must not initiate a paired lookup"
                        );
                        let mut record = crate::test_helpers::sparse_shared_asset_record();
                        record["recordName"] = json!(name);
                        record["zoneID"] = zone.clone();
                        record
                    })
                    .collect()
            } else {
                Self::pairs(zone)
                    .into_iter()
                    .filter(|r| names.iter().any(|n| r["recordName"] == *n))
                    .collect()
            };
            self.event(json!({"kind":"lookup_end","source":source,"returned":records.len(),"sparse":source=="PrimarySync"}));
            return Ok(json!({"records":records}));
        }
        assert!(url.contains("/changes/zone?"));
        let entry = &body["zones"][0];
        assert_eq!(entry["resultsLimit"], 200);
        let zone = &entry["zoneID"];
        let source = zone["zoneName"].as_str().unwrap();
        let page = entry["syncToken"]
            .as_str()
            .map(|s| s.strip_prefix("cursor-").unwrap().parse::<usize>().unwrap())
            .unwrap_or(0);
        let attempt = {
            let mut attempts = self.attempts.lock().unwrap();
            let attempt = attempts.entry((source.to_owned(), page)).or_default();
            *attempt += 1;
            *attempt
        };
        self.event(json!({"kind":"page_start","source":source,"page":page,"attempt":attempt}));
        if page == PREFIX_PAGES
            && self.gate_phase == "changes"
            && let Some(gate) = &self.gate
        {
            gate.notify_one();
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(Duration::from_secs(self.page_seconds)).await;
        if self.retry_page == Some(page) && (attempt == 1 || self.persistent_error) {
            self.event(json!({"kind":"page_error","source":source,"page":page,"attempt":attempt,"status":503}));
            return Err(crate::icloud::photos::session::HttpStatusError {
                status: 503,
                url: "https://example.invalid/synthetic".to_owned(),
                retry_after: Some(Duration::from_secs(self.retry_delay_secs)),
                body: None,
            }
            .into());
        }
        let records: Vec<_> = if page < PREFIX_PAGES {
            let start = page * 200;
            (start..(start+200).min(PREFIX_RECORDS)).map(|i| json!({
                "recordName":format!("unrelated-{source}-{i}"),"recordType":"CPLMaster","fields":{}})).collect()
        } else {
            assert!(self.late_owner && page == PREFIX_PAGES);
            Self::pairs(zone)
        };
        let more = page + 1 < PREFIX_PAGES
            || self.late_owner
            || (self.gate.is_some() && self.gate_phase == "changes");
        self.event(json!({"kind":"page_end","source":source,"page":page,"attempt":attempt,"records":records.len(),"more":more}));
        Ok(
            json!({"zones":[{"zoneID":zone,"syncToken":format!("cursor-{}",page+1),"moreComing":more,"records":records}]}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
fn targets() -> FxHashSet<String> {
    ["asset-LATE_A".to_owned(), "asset-LATE_B".to_owned()]
        .into_iter()
        .collect()
}

#[tokio::test(start_paused = true)]
async fn issue958_sparse_late_pair_and_retry_preserve_results() {
    for (name, late, shared, retry, expected_seconds) in [
        ("master_only_eof", false, false, false, 23),
        ("late_owner_pair", true, true, false, 24),
        ("late_owner_pair_retry", true, true, true, 27),
        ("owner_eof_then_shared_pair", false, true, false, 25),
    ] {
        let mut session = SparseFixture::new(1, late);
        session.retry_page = retry.then_some(5);
        let mut album = session.album("PrimarySync");
        if shared {
            album = album.with_cross_zone_sources(vec![session.album("SharedSync-fixture")]);
        }
        let mut missing = targets();
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let assets = album
            .hydrate_matching_assets_from_changes(&mut missing)
            .await
            .unwrap();
        assert_eq!(
            session.started.elapsed().as_secs(),
            expected_seconds,
            "{name}"
        );
        assert_eq!(assets.len(), if late || shared { 2 } else { 0 }, "{name}");
        assert_eq!(missing.len(), if late || shared { 0 } else { 2 }, "{name}");
        if late {
            assert!(
                assets
                    .iter()
                    .all(|a| a.source_zone() == Some("PrimarySync"))
            );
        } else if shared {
            assert!(
                assets
                    .iter()
                    .all(|a| a.source_zone() == Some("SharedSync-fixture"))
            );
        }
        let trace = session.trace.lock().unwrap().clone();
        if late {
            assert!(trace.iter().all(|e| e["source"] == "PrimarySync"));
        }
        let events = anonymous_events(&capture);
        let last = events.last().unwrap();
        assert_eq!(last.field("outcome"), Some("returned"));
        assert_eq!(last.field("retries"), Some(if retry { "1" } else { "0" }));
        assert_eq!(
            last.field("pages_received"),
            Some(if late { "23" } else { "22" })
        );
        assert_eq!(
            last.field("source_index"),
            Some(if shared && !late { "2" } else { "1" })
        );
        assert_eq!(
            last.field("phase"),
            Some(if shared && !late {
                "paired_lookup"
            } else {
                "changes"
            })
        );
    }
}

#[tokio::test(start_paused = true)]
async fn issue958_progress_distinguishes_page_latency_from_pending_request() {
    for (page_seconds, pending_start) in [(1, 23), (50, 1101)] {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let requested = Arc::new(tokio::sync::Notify::new());
        let mut session = SparseFixture::new(page_seconds, false);
        session.gate = Some(requested.clone());
        let album = session.album("PrimarySync");
        let mut missing = targets();
        let cancel = CancellationToken::new();
        let mut read = Box::pin(super::super::wait_for_hydration_read(
            album.hydrate_matching_assets_from_changes(&mut missing),
            "album_relation",
            &cancel,
        ));
        tokio::select! { ()=requested.notified()=>{}, r=read.as_mut()=>panic!("unexpected return: {r:?}") }
        assert_eq!(session.started.elapsed().as_secs(), pending_start);
        while session.started.elapsed() < Duration::from_secs(1177) {
            let next = ((session.started.elapsed().as_secs() / 30 + 1) * 30).min(1177);
            tokio::time::advance(Duration::from_secs(next) - session.started.elapsed()).await;
            tokio::select! { biased; r=read.as_mut()=>panic!("unexpected return: {r:?}"), ()=tokio::task::yield_now()=>{} }
        }
        cancel.cancel();
        assert!(read.as_mut().await.is_none());
        drop(read);
        assert_eq!(missing.len(), 2);
        assert!(
            capture.contains_event(|e| e.field("orphaned_masters") == Some("4282")
                && e.field("orphaned_assets") == Some("0"))
        );
        let heartbeats = capture
            .events()
            .iter()
            .filter(|e| {
                e.field("phase") == Some("album_relation") && e.field("outcome") == Some("waiting")
            })
            .count();
        assert_eq!(heartbeats, 39);
        let events = anonymous_events(&capture);
        assert_eq!(events.len(), 40); // One periodic event per 30s plus the drop summary.
        let first = events.first().unwrap();
        assert_eq!(
            first.field("pages_received"),
            Some(if page_seconds == 1 { "22" } else { "0" })
        );
        let last = events.last().unwrap();
        assert_eq!(last.field("outcome"), Some("interrupted"));
        assert_eq!(last.field("phase"), Some("changes"));
        assert_eq!(last.field("request_state"), Some("request"));
        assert_eq!(last.field("page"), Some("23"));
        assert_eq!(last.field("pages_received"), Some("22"));
        assert_eq!(last.field("records_received"), Some("4282"));
        assert_eq!(
            last.field("wait_secs"),
            Some(if page_seconds == 1 { "1154" } else { "76" })
        );
    }
}

fn anonymous_events(
    capture: &crate::test_helpers::TracingCapture,
) -> Vec<crate::test_helpers::CapturedEvent> {
    let events: Vec<_> = capture
        .events()
        .into_iter()
        .filter(|e| e.message() == Some("Album hydration progress"))
        .collect();
    assert!(!events.is_empty());
    let allowed = [
        "message",
        "outcome",
        "phase",
        "source_index",
        "source_role",
        "page",
        "pages_received",
        "records_received",
        "requests",
        "attempt",
        "retries",
        "request_state",
        "wait_secs",
        "elapsed_secs",
    ];
    for event in &events {
        assert!(
            event.fields.keys().all(|k| allowed.contains(&k.as_str())),
            "{event:?}"
        );
        let text = format!("{event:?}");
        for private in [
            "PrimarySync",
            "SharedSync-fixture",
            "asset-LATE",
            "cursor-",
            "example.invalid",
            "Synthetic album",
            "synthetic-album",
        ] {
            assert!(!text.contains(private), "{event:?}");
        }
    }
    events
}

#[tokio::test(start_paused = true)]
async fn issue958_progress_identifies_lookup_waits_without_source_names() {
    for phase in ["asset_lookup", "paired_lookup"] {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let requested = Arc::new(tokio::sync::Notify::new());
        let mut session = SparseFixture::new(1, false);
        session.gate = Some(requested.clone());
        session.gate_phase = phase;
        let album = session
            .album("PrimarySync")
            .with_cross_zone_sources(vec![session.album("SharedSync-fixture")]);
        let mut missing = targets();
        let mut read = Box::pin(album.hydrate_matching_assets_from_changes(&mut missing));
        tokio::select! { () = requested.notified() => {}, result = &mut read => panic!("unexpected return: {result:?}") }
        tokio::time::advance(Duration::from_secs(35)).await;
        tokio::select! { biased; result = &mut read => panic!("unexpected return: {result:?}"), () = tokio::task::yield_now() => {} }
        drop(read);
        assert_eq!(missing.len(), 2);
        let events = anonymous_events(&capture);
        let last = events.last().unwrap();
        assert_eq!(last.field("outcome"), Some("interrupted"));
        assert_eq!(last.field("phase"), Some(phase));
        assert_eq!(
            last.field("source_index"),
            Some(if phase == "asset_lookup" { "1" } else { "2" })
        );
        assert_eq!(
            last.field("source_role"),
            Some(if phase == "asset_lookup" {
                "owner"
            } else {
                "fallback"
            })
        );
        assert_eq!(last.field("page"), Some("0"));
        assert_eq!(last.field("request_state"), Some("request"));
        assert_eq!(last.field("wait_secs"), Some("35"));
    }
}

#[tokio::test(start_paused = true)]
async fn issue958_progress_identifies_retry_backoff_and_cancels_without_another_attempt() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let mut session = SparseFixture::new(1, true);
    session.retry_page = Some(5);
    session.retry_delay_secs = 120;
    let album = session.album("PrimarySync");
    let mut missing = targets();
    let mut read = Box::pin(album.hydrate_matching_assets_from_changes(&mut missing));
    tokio::select! { biased; result = &mut read => panic!("unexpected return: {result:?}"), () = tokio::time::sleep(Duration::from_secs(30)) => {} }
    // Poll the diagnostic tick without advancing to the next retry.
    tokio::select! { biased; result = &mut read => panic!("unexpected return: {result:?}"), () = tokio::task::yield_now() => {} }
    drop(read);
    assert_eq!(missing.len(), 2);
    let events = anonymous_events(&capture);
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(event.field("phase"), Some("changes"));
        assert_eq!(event.field("page"), Some("6"));
        assert_eq!(event.field("pages_received"), Some("5"));
        assert_eq!(event.field("request_state"), Some("between_attempts"));
        assert_eq!(event.field("attempt"), Some("1"));
        assert_eq!(event.field("retries"), Some("0"));
        assert_eq!(event.field("wait_secs"), Some("23"));
    }
    assert_eq!(events.last().unwrap().field("outcome"), Some("interrupted"));
    let trace = session.trace.lock().unwrap();
    assert_eq!(
        trace
            .iter()
            .filter(|e| e["kind"] == "page_start" && e["page"] == 5)
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn issue958_progress_reports_exhausted_retry_without_provider_values() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let mut session = SparseFixture::new(1, false);
    session.retry_page = Some(0);
    session.persistent_error = true;
    let album = session.album("PrimarySync");
    let mut missing = targets();
    let result = album
        .hydrate_matching_assets_from_changes(&mut missing)
        .await;
    assert!(result.is_err());
    assert_eq!(session.started.elapsed().as_secs(), 5);
    assert_eq!(missing.len(), 2);
    let events = anonymous_events(&capture);
    assert_eq!(events.len(), 1);
    let last = events.last().unwrap();
    assert_eq!(last.field("outcome"), Some("error"));
    assert_eq!(last.field("phase"), Some("changes"));
    assert_eq!(last.field("attempt"), Some("2"));
    assert_eq!(last.field("retries"), Some("1"));
    assert_eq!(last.field("pages_received"), Some("0"));
}

#[tokio::test(start_paused = true)]
async fn issue958_unscoped_provider_request_has_no_album_progress() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let session = SparseFixture::new(1, false);
    let body =
        json!({"zoneID":{"zoneName":"PrimarySync"},"records":[{"recordName":"asset-LATE_A"}]})
            .to_string();
    let result = crate::icloud::photos::session::retry_post(
        &session,
        "https://example.invalid/records/lookup?",
        &body,
        &[],
        &RetryConfig::default(),
    )
    .await;
    assert!(result.is_ok());
    assert!(!capture.contains_event(|e| e.message() == Some("Album hydration progress")));
}

#[tokio::test(start_paused = true)]
async fn issue958_concurrent_hydration_scopes_keep_counts_independent() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let fast = SparseFixture::new(1, true);
    let slow = SparseFixture::new(2, false);
    let fast_album = fast.album("PrimarySync");
    let slow_album = slow.album("PrimarySync");
    let mut fast_missing = targets();
    let mut slow_missing = targets();
    let (found, absent) = tokio::join!(
        fast_album.hydrate_matching_assets_from_changes(&mut fast_missing),
        slow_album.hydrate_matching_assets_from_changes(&mut slow_missing),
    );
    assert_eq!(found.unwrap().len(), 2);
    assert!(fast_missing.is_empty());
    assert!(absent.unwrap().is_empty());
    assert_eq!(slow_missing.len(), 2);
    let events = anonymous_events(&capture);
    let returned: Vec<_> = events
        .iter()
        .filter(|e| e.field("outcome") == Some("returned"))
        .collect();
    assert_eq!(returned.len(), 2);
    for (event, pages, records, requests, elapsed) in [
        (returned[0], "23", "4286", "24", "24"),
        (returned[1], "22", "4282", "23", "45"),
    ] {
        assert_eq!(event.field("pages_received"), Some(pages));
        assert_eq!(event.field("records_received"), Some(records));
        assert_eq!(event.field("requests"), Some(requests));
        assert_eq!(event.field("elapsed_secs"), Some(elapsed));
        assert_eq!(event.field("retries"), Some("0"));
    }
}
