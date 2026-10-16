use super::{Observation, RESPONSE_BYTES, classify};
use crate::icloud::photos::album::preservation_proof::tests::{BUDGET, masters};
use crate::icloud::photos::album::test_support::{
    canned_changes_page, changes_asset, changes_master, default_zone, make_album_with_session,
};
use crate::icloud::photos::session::{DiagnosticLookupError, PhotosSession};
use crate::test_helpers::TracingCapture;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

fn child(master: &str) -> Value {
    let mut r = changes_asset("private-child", master);
    r["zoneID"] = default_zone();
    r["fields"]["masterRef"]["value"]["zoneID"] = default_zone();
    r
}

#[test]
fn missing_reference_response_matrix_never_exposes_provider_values() {
    let live = child("master");
    let check = |r: Value, expected| {
        assert_eq!(
            classify(&r, "private-child", &default_zone(), &masters()),
            expected
        )
    };
    check(
        json!({"records":[live.clone()]}),
        ("live_reference", "candidate"),
    );
    check(
        json!({"records":[child("private-other-parent")]}),
        ("live_reference", "noncandidate"),
    );
    check(json!({"records":[]}), ("omitted", "not_established"));
    check(json!({}), ("malformed_response", "not_established"));
    check(
        json!({"records":[live.clone(),live.clone()]}),
        ("multiple_records", "not_established"),
    );
    for (field, value, outcome) in [
        (
            "recordName",
            json!("private-other-child"),
            "unexpected_record",
        ),
        ("recordType", json!("private-type"), "malformed_response"),
        ("deleted", json!(true), "deleted"),
        ("serverErrorCode", json!("private-error"), "provider_error"),
        (
            "zoneID",
            json!({"zoneName":"PrimarySync", "ownerRecordName":"foreign-private-owner"}),
            "foreign_scope",
        ),
        ("zoneID", Value::Null, "scope_unavailable"),
    ] {
        let mut r = live.clone();
        r[field] = value;
        check(json!({"records":[r]}), (outcome, "not_established"));
    }
    for value in [
        json!({}),
        json!(true),
        json!({"value":"private-deleted"}),
        json!({"value":2}),
    ] {
        let mut record = live.clone();
        record["fields"]["isDeleted"] = value;
        check(
            json!({"records":[record]}),
            ("malformed_response", "not_established"),
        );
    }
    for key in ["ownerRecordName", "zoneName"] {
        let mut record = live.clone();
        record["zoneID"][key] = json!(" ");
        check(
            json!({"records":[record]}),
            ("scope_unavailable", "not_established"),
        );
    }
    let mut missing = live.clone();
    missing["fields"]
        .as_object_mut()
        .unwrap()
        .remove("masterRef");
    check(
        json!({"records":[missing]}),
        ("missing_reference", "not_established"),
    );
    let mut malformed = live.clone();
    malformed["fields"]["masterRef"]["value"]["recordName"] = json!(42);
    check(
        json!({"records":[malformed]}),
        ("malformed_reference", "not_established"),
    );
    let mut foreign = live.clone();
    foreign["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] = json!("foreign");
    check(
        json!({"records":[foreign]}),
        ("foreign_scope", "not_established"),
    );
    let mut deleted = live.clone();
    deleted["fields"]["isDeleted"] = json!({"value":1});
    check(json!({"records":[deleted]}), ("deleted", "not_established"));
    check(
        json!({"records":[live],"private":"x".repeat(RESPONSE_BYTES)}),
        ("oversized_response", "not_established"),
    );
}

#[derive(Clone)]
struct Session {
    pages: Arc<Mutex<VecDeque<Value>>>,
    reply: Value,
    mode: &'static str,
    calls: Arc<AtomicUsize>,
    cancel: CancellationToken,
}
#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            return Ok(self.pages.lock().unwrap().pop_front().unwrap());
        }
        assert!(url.contains("/records/lookup?"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["zoneID"], default_zone());
        assert_eq!(body["records"].as_array().unwrap().len(), 1);
        assert_eq!(body["desiredKeys"].as_array().unwrap().len(), 6);
        match self.mode {
            "error" => anyhow::bail!("private-account https://private/?token=secret"),
            "cancel" => {
                self.cancel.cancel();
                std::future::pending().await
            }
            "hang" => std::future::pending().await,
            _ => {
                let mut reply = self.reply.clone();
                if let Some(record) = reply
                    .get_mut("records")
                    .and_then(Value::as_array_mut)
                    .and_then(|r| r.first_mut())
                {
                    record["recordName"] = body["records"][0]["recordName"].clone();
                }
                Ok(reply)
            }
        }
    }
    async fn post_diagnostic(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<Value, DiagnosticLookupError> {
        self.post(url, body, headers)
            .await
            .map_err(|_private_error| DiagnosticLookupError::Request)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn run_case(mode: &'static str, replacement: bool, reply: Value) -> (Value, usize) {
    let (capture, _guard) = TracingCapture::install();
    let mut sparse = child("master");
    sparse["fields"]
        .as_object_mut()
        .unwrap()
        .remove("masterRef");
    sparse["fields"]["isSparsePrivateRecord"] = json!({"value":1});
    sparse["fields"]["linkedShareRecordName"] = json!({"value":"private-link"});
    if mode == "long_identity" {
        sparse["recordName"] = json!("x".repeat(513));
    }
    let mut pages = VecDeque::new();
    if mode == "tracking_limit" {
        let records: Vec<Value> = (0..64)
            .map(|i| {
                let mut r = sparse.clone();
                r["recordName"] = json!(format!("prior-{i}"));
                r
            })
            .collect();
        pages.push_back(canned_changes_page(&records, "private-cap-cursor", true));
        // The 65th missing-reference identity is untracked; later tombstones
        // remove the first 64 so iteration must reach the untracked one.
        let mut records = vec![sparse.clone()];
        records.extend((0..64).map(|i| {
            let mut r = sparse.clone();
            r["recordName"] = json!(format!("prior-{i}"));
            r["deleted"] = json!(true);
            r
        }));
        pages.push_back(canned_changes_page(&records, "private-cap-final", false));
    }
    if replacement {
        pages.push_back(canned_changes_page(
            &[child("master")],
            "private-earlier-cursor",
            true,
        ));
    }
    // Multiple missing children still cause only one diagnostic lookup.
    let mut second = sparse.clone();
    second["recordName"] = json!("private-second");
    if mode != "tracking_limit" {
        pages.push_back(canned_changes_page(
            &if replacement || mode == "long_identity" {
                vec![changes_master("master"), sparse]
            } else {
                vec![changes_master("master"), sparse, second]
            },
            "private-final-cursor",
            false,
        ));
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let session = Session {
        pages: Arc::new(Mutex::new(pages)),
        reply,
        mode,
        calls: Arc::clone(&calls),
        cancel: cancel.clone(),
    };
    let album = make_album_with_session(100, Box::new(session));
    let error = album
        .legacy_preservation_inventory(
            &masters(),
            &cancel,
            if mode == "tracking_limit" {
                super::super::InventoryBudget {
                    records: 200,
                    retained_bytes: 128 * 1024,
                    ..BUDGET
                }
            } else {
                BUDGET
            },
        )
        .await
        .unwrap_err();
    let d = super::super::classify_legacy_inventory_error(&error).unwrap();
    assert_eq!(d.subreason, "child_master_reference_missing");
    assert!(d.eof_observed);
    assert_eq!(d.family_context, "unknown");
    let events = capture.events();
    let event = events
        .iter()
        .find(|e| e.field("diagnostic") == Some("support_missing_reference_v1"))
        .unwrap();
    let fields: Value = serde_json::from_str(event.field("support_fields").unwrap()).unwrap();
    let printed = fields.to_string();
    assert!(!printed.contains("private"));
    assert!(!printed.contains("secret"));
    (fields, calls.load(Ordering::SeqCst))
}

#[tokio::test]
async fn missing_reference_lookup_is_bounded_and_never_repairs_inventory() {
    // Both a useful lookup and every failure leave the same original refusal.
    for (mode, reply, expected) in [
        ("ok", json!({"records":[]}), "omitted"),
        ("ok", json!({"records":[child("master")]}), "live_reference"),
        ("error", Value::Null, "request_failed"),
        ("cancel", Value::Null, "cancelled"),
        ("hang", Value::Null, "timeout"),
    ] {
        let (f, calls) = run_case(mode, false, reply).await;
        assert_eq!(f["lookup_outcome"], expected);
        assert_eq!(f["record_history"], "first_seen");
        assert_eq!(f["prior_reference"], "not_observed");
        assert_eq!(f["sparse_marker"], "true");
        assert_eq!(f["linkage"], "partial_fields");
        assert_eq!(calls, 1);
    }
}

#[test]
fn missing_reference_observations_describe_only_immediate_retained_history() {
    let mut first: super::Record = serde_json::from_value(child("master")).unwrap();
    let mut replacement = first.clone();
    replacement
        .fields
        .as_object_mut()
        .unwrap()
        .remove("masterRef");
    let o = Observation::new(&replacement, Some(&first));
    assert_eq!(o.history, "replacement");
    assert_eq!(o.prior_reference, "present");
    first.fields["masterRef"] = json!({"value":null});
    assert_eq!(
        Observation::new(&replacement, Some(&first)).prior_reference,
        "malformed"
    );
    assert_eq!(
        Observation::new(&replacement, Some(&replacement)).prior_reference,
        "missing"
    );
    assert_eq!(
        Observation::new(&replacement, None).prior_reference,
        "not_observed"
    );
}

#[tokio::test]
async fn missing_reference_replacement_lookup_keeps_original_refusal() {
    let (fields, calls) = run_case("ok", true, json!({"records":[child("master")]})).await;
    assert_eq!(calls, 1);
    assert_eq!(fields["record_history"], "replacement");
    assert_eq!(fields["prior_reference"], "present");
    assert_eq!(fields["lookup_outcome"], "live_reference");
    assert_eq!(fields["lookup_family"], "candidate");
}

#[test]
fn missing_reference_compacted_prior_is_unavailable() {
    let mut prior: super::Record = serde_json::from_value(child("master")).unwrap();
    let current = prior.clone();
    prior.deleted = Some(true);
    prior.fields = Value::Null;
    assert_eq!(
        Observation::new(&current, Some(&prior)).prior_reference,
        "unavailable"
    );
    assert_eq!(Observation::unavailable().history, "not_recorded");
}

#[tokio::test]
async fn missing_reference_tracking_limits_remain_explicit() {
    for mode in ["long_identity", "tracking_limit"] {
        let (fields, calls) = run_case(mode, false, json!({"records":[]})).await;
        assert_eq!(calls, 1);
        assert_eq!(fields["record_history"], "not_recorded");
        assert_eq!(fields["sparse_marker"], "not_recorded");
        assert_eq!(fields["lookup_outcome"], "omitted");
    }
}

#[tokio::test]
async fn missing_reference_precancelled_probe_starts_no_request() {
    let (capture, _guard) = TracingCapture::install();
    let calls = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let session = Session {
        pages: Arc::new(Mutex::new(VecDeque::new())),
        reply: Value::Null,
        mode: "ok",
        calls: Arc::clone(&calls),
        cancel: cancel.clone(),
    };
    let album = make_album_with_session(100, Box::new(session));
    let record = serde_json::from_value(child("master")).unwrap();
    album
        .observe_missing_reference(&record, Observation::unavailable(), &masters(), &cancel)
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(capture.events().iter().any(|event| {
        event
            .field("support_fields")
            .is_some_and(|value| value.contains("cancelled"))
    }));
}
