//! Observations only. A later lookup never repairs inventory at an earlier cursor.
use std::time::Duration;

use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{PhotoAlbum, Record, encode_params, serialized_bytes};
use crate::icloud::photos::session::DiagnosticLookupError;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Observation {
    history: &'static str,
    prior_reference: &'static str,
    sparse_marker: &'static str,
    linkage: &'static str,
}

impl Observation {
    pub(super) fn unavailable() -> Self {
        Self {
            history: "not_recorded",
            prior_reference: "not_observed",
            sparse_marker: "not_recorded",
            linkage: "not_recorded",
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
                "replacement"
            } else {
                "first_seen"
            },
            prior_reference: prior.map_or("not_observed", |r| {
                if r.deleted == Some(true) || r.record_type != "CPLAsset" {
                    "unavailable"
                } else {
                    reference_shape(&r.fields)
                }
            }),
            sparse_marker: match marker {
                None => "absent",
                Some(v) if v.as_bool() == Some(true) || v.as_i64() == Some(1) => "true",
                Some(v) if v.as_bool() == Some(false) || v.as_i64() == Some(0) => "false",
                Some(_) => "malformed",
            },
            linkage: match linkage {
                0 => "none",
                3 => "all_fields",
                _ => "partial_fields",
            },
        }
    }
}

fn reference_shape(fields: &Value) -> &'static str {
    match fields.pointer("/masterRef/value") {
        None => "missing",
        Some(v)
            if v.get("recordName")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty()) =>
        {
            "present"
        }
        Some(_) => "malformed",
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

fn classify(
    response: &Value,
    child: &str,
    expected: &Value,
    masters: &FxHashSet<String>,
) -> (&'static str, &'static str) {
    let unknown = "not_established";
    if serialized_bytes(response).map_or(true, |n| n > RESPONSE_BYTES) {
        return ("oversized_response", unknown);
    }
    let Some(records) = response.get("records").and_then(Value::as_array) else {
        return ("malformed_response", unknown);
    };
    let record = match records.as_slice() {
        [] => return ("omitted", unknown),
        [record] => record,
        _ => return ("multiple_records", unknown),
    };
    if record.get("recordName").and_then(Value::as_str) != Some(child) {
        return ("unexpected_record", unknown);
    }
    let checked = scope(record.get("zoneID"), expected);
    if checked != "matched" {
        return (checked, unknown);
    }
    if record.get("serverErrorCode").is_some_and(|v| !v.is_null()) {
        return ("provider_error", unknown);
    }
    if record
        .get("deleted")
        .is_some_and(|value| !value.is_boolean())
    {
        return ("malformed_response", unknown);
    }
    if record.get("deleted") == Some(&Value::Bool(true)) {
        return ("deleted", unknown);
    }
    if record.get("recordType").and_then(Value::as_str) != Some("CPLAsset") {
        return ("malformed_response", unknown);
    }
    if let Some(deleted) = record.pointer("/fields/isDeleted") {
        match deleted.get("value").and_then(Value::as_i64) {
            Some(1) => return ("deleted", unknown),
            Some(0) => {}
            _ => return ("malformed_response", unknown),
        }
    }
    let Some(reference) = record.pointer("/fields/masterRef/value") else {
        return ("missing_reference", unknown);
    };
    let Some(master) = reference
        .get("recordName")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    else {
        return ("malformed_reference", unknown);
    };
    let checked = scope(reference.get("zoneID"), expected);
    if checked != "matched" {
        return (checked, unknown);
    }
    (
        "live_reference",
        if masters.contains(master) {
            "candidate"
        } else {
            "noncandidate"
        },
    )
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
        let (outcome, family) = tokio::select! {
            biased;
            () = cancel.cancelled() => ("cancelled", "not_established"),
            response = tokio::time::timeout(LOOKUP_TIMEOUT, self.session.post_diagnostic(&url, body, &[("Content-type", "text/plain")])) => match response {
                Err(_) => ("timeout", "not_established"),
                Ok(Err(error)) => (match error {
                    DiagnosticLookupError::Unavailable => "transport_unavailable",
                    DiagnosticLookupError::Request => "request_failed",
                    DiagnosticLookupError::HttpStatus => "http_error",
                    DiagnosticLookupError::TooLarge => "oversized_response",
                    DiagnosticLookupError::Malformed => "malformed_response",
                }, "not_established"),
                Ok(Ok(value)) => classify(&value, &record.record_name, &self.zone_id, masters),
            },
        };
        crate::support::observe(
            "support_missing_reference_v1",
            json!({
                "record_history":observation.history, "prior_reference":observation.prior_reference,
                "sparse_marker":observation.sparse_marker, "linkage":observation.linkage,
                "lookup_outcome":outcome, "lookup_family":family,
            }),
        );
    }
}

#[cfg(test)]
mod tests;
