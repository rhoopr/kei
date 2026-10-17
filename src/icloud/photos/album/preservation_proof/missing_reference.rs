//! Observations only. A later lookup never repairs inventory at an earlier cursor.
use std::time::Duration;

use rustc_hash::FxHashSet;
use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{PhotoAlbum, Record, encode_params, serialized_bytes};
use crate::icloud::photos::session::DiagnosticLookupError;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
enum RecordHistory {
    NotRecorded,
    FirstSeen,
    Replacement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
enum ReferenceShape {
    NotObserved,
    Unavailable,
    Missing,
    Present,
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
enum SparseMarker {
    NotRecorded,
    Absent,
    True,
    False,
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
enum LinkageShape {
    NotRecorded,
    None,
    PartialFields,
    AllFields,
}

#[derive(Clone, Copy)]
pub(super) struct Observation {
    history: RecordHistory,
    prior_reference: ReferenceShape,
    sparse_marker: SparseMarker,
    linkage: LinkageShape,
}

impl Observation {
    pub(super) fn unavailable() -> Self {
        Self {
            history: RecordHistory::NotRecorded,
            prior_reference: ReferenceShape::NotObserved,
            sparse_marker: SparseMarker::NotRecorded,
            linkage: LinkageShape::NotRecorded,
        }
    }

    pub(super) fn new(record: &Record, prior: Option<&Record>) -> Self {
        let marker = record.fields.pointer("/isSparsePrivateRecord/value");
        let linkage = [
            "linkedShareRecordName",
            "linkedShareZoneName",
            "linkedShareZoneOwner",
        ]
        .iter()
        .filter(|key| record.fields.get(**key).is_some())
        .count();
        Self {
            history: if prior.is_some() {
                RecordHistory::Replacement
            } else {
                RecordHistory::FirstSeen
            },
            prior_reference: prior.map_or(ReferenceShape::NotObserved, |r| {
                if r.deleted == Some(true) || r.record_type != "CPLAsset" {
                    ReferenceShape::Unavailable
                } else {
                    reference_shape(&r.fields)
                }
            }),
            sparse_marker: match marker {
                None => SparseMarker::Absent,
                Some(v) if v.as_bool() == Some(true) || v.as_i64() == Some(1) => SparseMarker::True,
                Some(v) if v.as_bool() == Some(false) || v.as_i64() == Some(0) => {
                    SparseMarker::False
                }
                Some(_) => SparseMarker::Malformed,
            },
            linkage: match linkage {
                0 => LinkageShape::None,
                3 => LinkageShape::AllFields,
                _ => LinkageShape::PartialFields,
            },
        }
    }
}

fn reference_shape(fields: &Value) -> ReferenceShape {
    match fields.pointer("/masterRef/value") {
        None => ReferenceShape::Missing,
        Some(v)
            if v.get("recordName")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty()) =>
        {
            ReferenceShape::Present
        }
        Some(_) => ReferenceShape::Malformed,
    }
}

// Require explicit scope before describing membership. Missing scope is not a
// foreign-owner assertion, and an exact request is not evidence of response scope.
fn scope(zone: Option<&Value>, expected: &Value) -> &'static str {
    let Some(zone) = zone else {
        return "scope_unavailable";
    };
    if [zone, expected].iter().any(|scope| {
        ["zoneName", "ownerRecordName"].iter().any(|key| {
            scope
                .get(*key)
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
        })
    }) {
        return "scope_unavailable";
    }
    if zone.get("zoneName") != expected.get("zoneName")
        || zone.get("ownerRecordName") != expected.get("ownerRecordName")
    {
        return "foreign_scope";
    }
    "matched"
}

#[derive(Debug)]
struct LookupObservation {
    outcome: &'static str,
    family: &'static str,
    scope: &'static str,
    shape: &'static str,
}

impl LookupObservation {
    fn unavailable(outcome: &'static str) -> Self {
        Self {
            outcome,
            family: "not_established",
            scope: "not_observed",
            shape: "not_observed",
        }
    }
}

// Shape describes only the matching response payload. It never establishes scope
// or family membership, even for a provider error or a deletion flag.
fn record_shape(record: &Value) -> &'static str {
    if record.get("serverErrorCode").is_some_and(|v| !v.is_null()) {
        return "provider_error";
    }
    if record
        .get("deleted")
        .is_some_and(|value| !value.is_boolean())
    {
        return "malformed_response";
    }
    if record.get("deleted") == Some(&Value::Bool(true)) {
        return "deleted";
    }
    if record.get("recordType").and_then(Value::as_str) != Some("CPLAsset") {
        return "malformed_response";
    }
    if let Some(deleted) = record.pointer("/fields/isDeleted") {
        match deleted.get("value").and_then(Value::as_i64) {
            Some(1) => return "deleted",
            Some(0) => {}
            _ => return "malformed_response",
        }
    }
    let Some(reference) = record.pointer("/fields/masterRef/value") else {
        return "missing_reference";
    };
    if reference
        .get("recordName")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty())
    {
        return "malformed_reference";
    }
    "live_reference"
}

fn classify(
    response: &Value,
    child: &str,
    expected: &Value,
    masters: &FxHashSet<String>,
) -> LookupObservation {
    if serialized_bytes(response).map_or(true, |n| n > RESPONSE_BYTES) {
        return LookupObservation::unavailable("oversized_response");
    }
    let Some(records) = response.get("records").and_then(Value::as_array) else {
        return LookupObservation::unavailable("malformed_response");
    };
    let record = match records.as_slice() {
        [] => return LookupObservation::unavailable("omitted"),
        [record] => record,
        _ => return LookupObservation::unavailable("multiple_records"),
    };
    if record.get("recordName").and_then(Value::as_str) != Some(child) {
        return LookupObservation::unavailable("unexpected_record");
    }
    let shape = record_shape(record);
    let mut observation = LookupObservation {
        outcome: shape,
        family: "not_established",
        scope: "matched",
        shape,
    };
    let checked = scope(record.get("zoneID"), expected);
    if checked != "matched" {
        observation.outcome = checked;
        observation.scope = if scope(Some(expected), expected) != "matched" {
            "expected_unavailable"
        } else if checked == "foreign_scope" {
            "child_foreign"
        } else {
            "child_unavailable"
        };
        return observation;
    }
    if observation.shape != "live_reference" {
        return observation;
    }
    if let Some(reference) = record.pointer("/fields/masterRef/value") {
        let checked = scope(reference.get("zoneID"), expected);
        if checked != "matched" {
            observation.outcome = checked;
            observation.scope = if checked == "foreign_scope" {
                "reference_foreign"
            } else {
                "reference_unavailable"
            };
        } else if let Some(master) = reference.get("recordName").and_then(Value::as_str) {
            observation.family = if masters.contains(master) {
                "candidate"
            } else {
                "noncandidate"
            };
        }
    }
    observation
}

impl PhotoAlbum {
    // One lookup operation at the first refusal, no application retries or paired fetch,
    // no returned record retention and no input to inventory/proof policy.
    pub(super) async fn observe_missing_reference(
        &self,
        record: &Record,
        observation: Observation,
        masters: &FxHashSet<String>,
        cancel: &CancellationToken,
    ) {
        let url = format!(
            "{}/records/lookup?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({"zoneID":self.zone_id.as_ref(), "records":[{"recordName":record.record_name}],
            "desiredKeys":["masterRef","isDeleted","isSparsePrivateRecord","linkedShareRecordName","linkedShareZoneName","linkedShareZoneOwner"]}).to_string();
        let lookup = tokio::select! {
            biased;
            () = cancel.cancelled() => LookupObservation::unavailable("cancelled"),
            response = tokio::time::timeout(LOOKUP_TIMEOUT, self.session.post_diagnostic(&url, body, &[("Content-type", "text/plain")])) => match response {
                Err(_) => LookupObservation::unavailable("timeout"),
                Ok(Err(error)) => LookupObservation::unavailable(match error {
                    DiagnosticLookupError::Unavailable => "transport_unavailable",
                    DiagnosticLookupError::Request => "request_failed",
                    DiagnosticLookupError::HttpStatus => "http_error",
                    DiagnosticLookupError::TooLarge => "oversized_response",
                    DiagnosticLookupError::Malformed => "malformed_response",
                }),
                Ok(Ok(value)) => classify(&value, &record.record_name, &self.zone_id, masters),
            },
        };
        crate::support::observe(
            "support_missing_reference_v1",
            json!({
                "record_history":observation.history, "prior_reference":observation.prior_reference,
                "sparse_marker":observation.sparse_marker, "linkage":observation.linkage,
                "lookup_outcome":lookup.outcome, "lookup_family":lookup.family,
                "lookup_scope":lookup.scope, "lookup_shape":lookup.shape,
            }),
        );
    }
}

#[cfg(test)]
mod tests;
