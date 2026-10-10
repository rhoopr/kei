//! Targeted record lookup and conservative identity resolution.

use super::PhotoAlbum;
use crate::icloud::photos::asset::SparseShareEvidence;
use crate::icloud::photos::asset::{PhotoAsset, RequiredAssetFields, extract_master_ref};
use crate::icloud::photos::cloudkit;
use crate::icloud::photos::queries::IDENTITY_LOOKUP_KEYS_VALUES;
use crate::icloud::photos::queries::encode_params;
use crate::icloud::photos::session;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::sync::Arc;

const RECORD_LOOKUP_BATCH_SIZE: usize = 100;

// Only fixed labels leave this trust boundary. Never log provider record types,
// error messages, identifiers, reference values, or response bodies here.
fn asset_identity_diagnostic(
    record: Option<&Value>,
    lookup_zone: &Value,
) -> (&'static str, &'static str) {
    let Some(record) = record else {
        return ("record_omitted", "absent");
    };
    let reference = record.pointer("/fields/masterRef/value");
    let reference_zone = match reference.and_then(|value| value.get("zoneID")) {
        None => "absent",
        Some(zone) if !zone.is_object() => "malformed",
        Some(zone) if zone == lookup_zone => "same_zone",
        Some(_) => "different_or_partial_zone",
    };
    let reason = if record
        .get("serverErrorCode")
        .is_some_and(|code| !code.is_null())
    {
        match record.get("serverErrorCode").and_then(Value::as_str) {
            Some("UNKNOWN_ITEM" | "NOT_FOUND") => "record_not_found",
            Some("ACCESS_DENIED" | "AUTHENTICATION_REQUIRED") => "record_access_denied",
            Some("ZONE_NOT_FOUND") => "record_zone_not_found",
            _ => "record_provider_error",
        }
    } else if record.get("recordType").and_then(Value::as_str) != Some("CPLAsset") {
        "unexpected_record_type"
    } else if serde_json::from_value::<cloudkit::Record>(record.clone()).is_err() {
        "record_decode_failed"
    } else if reference.is_none() {
        record
            .get("fields")
            .and_then(SparseShareEvidence::from_fields)
            .map_or("master_reference_missing", |evidence| evidence.diagnostic())
    } else if reference
        .and_then(|value| value.get("recordName"))
        .and_then(Value::as_str)
        .is_none_or(|name| name.trim().is_empty())
    {
        "master_reference_malformed"
    } else {
        "master_reference_present"
    };
    (reason, reference_zone)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProviderRecordId(Arc<str>);

impl ProviderRecordId {
    pub(crate) fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordLookupRequest {
    pub(crate) state_id: ProviderRecordId,
    pub(crate) master_record_name: ProviderRecordId,
    pub(crate) asset_record_name: Option<ProviderRecordId>,
    target: RecordLookupTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordLookupTarget {
    Master,
    Asset,
}

impl RecordLookupRequest {
    pub(crate) fn paired(
        state_id: ProviderRecordId,
        master_record_name: ProviderRecordId,
        asset_record_name: ProviderRecordId,
    ) -> Self {
        Self {
            state_id,
            master_record_name,
            asset_record_name: Some(asset_record_name),
            target: RecordLookupTarget::Master,
        }
    }

    pub(crate) fn master_only(
        state_id: ProviderRecordId,
        master_record_name: ProviderRecordId,
    ) -> Self {
        Self {
            state_id,
            master_record_name,
            asset_record_name: None,
            target: RecordLookupTarget::Master,
        }
    }

    pub(crate) fn asset_only(asset_record_name: ProviderRecordId) -> Self {
        Self {
            state_id: asset_record_name.clone(),
            master_record_name: asset_record_name,
            asset_record_name: None,
            target: RecordLookupTarget::Asset,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProviderLookupError {
    #[error("provider authentication failed with HTTP {status}: {message}")]
    Authentication { status: u16, message: String },
    #[error("provider record lookup was rate limited with HTTP {status}: {message}")]
    RateLimited { status: u16, message: String },
    #[error("provider record lookup request failed: {0}")]
    Request(String),
    #[error("provider record lookup response was malformed: {0}")]
    Malformed(String),
}

impl ProviderLookupError {
    #[must_use]
    pub(crate) fn is_authentication(&self) -> bool {
        matches!(self, Self::Authentication { .. })
    }

    /// Bounded diagnostic safe for normal logs; excludes provider response text.
    pub(crate) fn diagnostic(&self) -> &'static str {
        match self {
            Self::Authentication { .. } => "authentication",
            Self::RateLimited { .. } => "rate_limited",
            Self::Request(_) => "request_failed",
            Self::Malformed(_) => "malformed_response",
        }
    }
}

fn classify_provider_lookup_error(error: &anyhow::Error) -> ProviderLookupError {
    let message = error.to_string();
    let Some(http) = error.downcast_ref::<session::HttpStatusError>() else {
        return ProviderLookupError::Request(message);
    };
    match http.status {
        401 | 403 | 421 => ProviderLookupError::Authentication {
            status: http.status,
            message,
        },
        429 | 503 => ProviderLookupError::RateLimited {
            status: http.status,
            message,
        },
        _ => ProviderLookupError::Request(message),
    }
}

#[derive(Debug)]
pub(crate) enum RecordResolution {
    Present(PhotoAsset),
    AssetPresent {
        master_record_name: ProviderRecordId,
    },
    MasterPresent,
    SparseShareUnresolved(SparseShareEvidence),
    Deleted {
        deleted_at: Option<chrono::DateTime<chrono::Utc>>,
        master_family: bool,
    },
    Unknown,
    TransientFailure(ProviderLookupError),
}

#[derive(Debug)]
pub(crate) struct RecordResolutionBatch {
    pub(crate) results: Vec<(ProviderRecordId, RecordResolution)>,
    pub(crate) complete: bool,
    pub(crate) rate_limit_observations: usize,
    /// Local response observation for each present resource, not provider issuance or expiry.
    pub(crate) url_observed_at: FxHashMap<ProviderRecordId, std::time::Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ResolutionEvidence {
    ChildDeleted,
    Inconclusive,
    MasterPresent,
    AssetPresent,
    MasterDeleted,
    Present,
}

fn resolution_evidence(resolution: &RecordResolution) -> ResolutionEvidence {
    match resolution {
        RecordResolution::Present(_) => ResolutionEvidence::Present,
        RecordResolution::AssetPresent { .. } => ResolutionEvidence::AssetPresent,
        RecordResolution::MasterPresent => ResolutionEvidence::MasterPresent,
        RecordResolution::Deleted {
            master_family: true,
            ..
        } => ResolutionEvidence::MasterDeleted,
        RecordResolution::SparseShareUnresolved(_)
        | RecordResolution::Unknown
        | RecordResolution::TransientFailure(_) => ResolutionEvidence::Inconclusive,
        RecordResolution::Deleted {
            master_family: false,
            ..
        } => ResolutionEvidence::ChildDeleted,
    }
}

// Missing optional zone fields inherit the authenticated lookup request scope.
// Every supplied field, including an owner, must agree. A bare or malformed
// zone cannot redirect lookup to another account or library.
fn lookup_zone_matches(actual: &Value, expected: &Value) -> bool {
    actual.as_object().is_some_and(|fields| {
        fields
            .get("zoneName")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.trim().is_empty())
            && fields
                .iter()
                .all(|(key, value)| expected.get(key) == Some(value))
    })
}

fn supplied_owner_shape(
    master: Option<&Value>,
    asset: Option<&Value>,
    stage: &str,
    expected: &Value,
) -> (&'static str, &'static str) {
    let scope = master
        .into_iter()
        .chain(asset)
        .filter_map(|record| {
            if stage == "master_reference_scope" {
                record.pointer("/fields/masterRef/value/zoneID")
            } else {
                record.get("zoneID")
            }
        })
        .find(|scope| !lookup_zone_matches(scope, expected));
    match scope.and_then(|v| v.get("ownerRecordName")) {
        None => ("absent", "absent"),
        Some(Value::Null) => ("null", "null"),
        Some(Value::String(s)) => (
            if s.is_empty() {
                "empty"
            } else if s == "_defaultOwner" {
                "private_default"
            } else {
                "other_string"
            },
            "string",
        ),
        Some(Value::Number(_)) => ("malformed", "number"),
        Some(Value::Bool(_)) => ("malformed", "boolean"),
        Some(Value::Array(_)) => ("malformed", "array"),
        Some(Value::Object(_)) => ("malformed", "object"),
    }
}

// Classify only a rejected scope. Acceptance remains owned by the matcher above.
fn lookup_scope_rejection(actual: &Value, expected: &Value) -> &'static str {
    if !actual.is_object() {
        "malformed_scope"
    } else if actual
        .get("zoneName")
        .and_then(Value::as_str)
        .is_none_or(|name| name.trim().is_empty())
    {
        "partial_scope"
    } else if actual.get("zoneName") != expected.get("zoneName") {
        "zone_conflict"
    } else if actual.get("ownerRecordName").is_some() && expected.get("ownerRecordName").is_none() {
        "owner_unqualified"
    } else if actual
        .get("ownerRecordName")
        .is_some_and(|owner| expected.get("ownerRecordName") != Some(owner))
    {
        "owner_conflict"
    } else {
        "scope_component_conflict"
    }
}

fn lookup_rejection(
    master: Option<&Value>,
    asset: Option<&Value>,
    request: &RecordLookupRequest,
    expected: &Value,
    pair_matches: bool,
) -> (&'static str, &'static str) {
    for record in master.into_iter().chain(asset) {
        for (zone, component) in [
            (record.get("zoneID"), "record_scope"),
            (
                record.pointer("/fields/masterRef/value/zoneID"),
                "master_reference_scope",
            ),
        ] {
            if let Some(zone) = zone
                && !lookup_zone_matches(zone, expected)
            {
                return (lookup_scope_rejection(zone, expected), component);
            }
        }
    }
    if !pair_matches {
        ("child_master_mismatch", "pairing")
    } else if master.into_iter().chain(asset).any(|record| {
        record
            .get("serverErrorCode")
            .is_some_and(|code| !code.is_null())
    }) {
        ("record_provider_error", "record_error")
    } else if master.is_none() {
        (
            if request.target == RecordLookupTarget::Asset {
                "child_record_omitted"
            } else {
                "master_record_omitted"
            },
            "record_presence",
        )
    } else if request.asset_record_name.is_some() && asset.is_none() {
        ("child_record_omitted", "record_presence")
    } else {
        ("record_identity_incomplete", "record_decode_or_reference")
    }
}

// Multiple provider records can map to one durable state identity. Merge them
// conservatively: a present sibling resolves the work, a missing master proves
// family deletion, and any inconclusive sibling blocks child-only deletion.
fn merge_record_resolution(existing: &mut RecordResolution, incoming: RecordResolution) {
    if resolution_evidence(&incoming) > resolution_evidence(existing)
        || (matches!(existing, RecordResolution::Unknown)
            && matches!(incoming, RecordResolution::SparseShareUnresolved(_)))
    {
        *existing = incoming;
    }
}

impl PhotoAlbum {
    /// Resolve durable pending identities without scanning the surrounding
    /// album or library. Missing response members are inconclusive; only an
    /// explicit CloudKit not-found result or tombstone is deletion evidence.
    pub(super) async fn lookup_records(
        &self,
        requests: &[RecordLookupRequest],
    ) -> RecordResolutionBatch {
        let mut results = Vec::with_capacity(requests.len());
        let mut identity_diagnostics = std::collections::BTreeMap::new();
        let mut rejection_diagnostics = std::collections::BTreeMap::new();
        let mut rate_limit_observations = 0usize;
        let mut url_observed_at = FxHashMap::default();
        let url = format!(
            "{}/records/lookup?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );

        for batch in requests.chunks(RECORD_LOOKUP_BATCH_SIZE) {
            let mut record_names = FxHashSet::default();
            let mut records = Vec::with_capacity(batch.len().saturating_mul(2));
            for request in batch {
                for record_id in std::iter::once(&request.master_record_name)
                    .chain(request.asset_record_name.as_ref())
                {
                    if record_names.insert(record_id.as_str().to_string()) {
                        records.push(json!({
                            "recordName": record_id.as_str(),
                        }));
                    }
                }
            }
            let body = json!({
                "records": records,
                "zoneID": self.zone_id.as_ref(),
                "desiredKeys": &*IDENTITY_LOOKUP_KEYS_VALUES,
            });
            let retried = session::retry_post_allowing_record_errors(
                self.session.as_ref(),
                &url,
                &body.to_string(),
                &[("Content-type", "text/plain")],
                &self.retry_config,
            )
            .await;
            rate_limit_observations =
                rate_limit_observations.saturating_add(retried.rate_limit_observations);
            let response = match retried.response {
                Ok(response) => response,
                Err(error) => {
                    let error = classify_provider_lookup_error(&error);
                    crate::metrics::record_targeted_lookup("transient_failure", batch.len());
                    results.extend(batch.iter().map(|request| {
                        (
                            request.state_id.clone(),
                            RecordResolution::TransientFailure(error.clone()),
                        )
                    }));
                    continue;
                }
            };

            let observed_at = std::time::Instant::now();
            let Some(response_records) = response.get("records").and_then(Value::as_array) else {
                crate::metrics::record_targeted_lookup("transient_failure", batch.len());
                results.extend(batch.iter().map(|request| {
                    (
                        request.state_id.clone(),
                        RecordResolution::TransientFailure(ProviderLookupError::Malformed(
                            "missing records array".to_string(),
                        )),
                    )
                }));
                continue;
            };
            let by_name: FxHashMap<&str, &Value> = response_records
                .iter()
                .filter_map(|record| {
                    record
                        .get("recordName")
                        .and_then(Value::as_str)
                        .map(|name| (name, record))
                })
                .collect();

            for request in batch {
                let master = by_name.get(request.master_record_name.as_str()).copied();
                let asset = request
                    .asset_record_name
                    .as_ref()
                    .and_then(|record_name| by_name.get(record_name.as_str()).copied());
                let explicit_not_found = |record: Option<&Value>| {
                    record
                        .and_then(|record| record.get("serverErrorCode"))
                        .and_then(Value::as_str)
                        .is_some_and(|code| matches!(code, "UNKNOWN_ITEM" | "NOT_FOUND"))
                };
                let tombstoned = |record: Option<&Value>| {
                    record
                        .and_then(|record| record.get("deleted"))
                        .and_then(Value::as_bool)
                        == Some(true)
                };
                let deleted_at = master.into_iter().chain(asset).find_map(|record| {
                    record
                        .get("fields")
                        .and_then(|fields| fields.get("deletedDate"))
                        .and_then(|field| field.get("value"))
                        .and_then(Value::as_i64)
                        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                });

                let primary_deleted = explicit_not_found(master) || tombstoned(master);
                let asset_deleted = explicit_not_found(asset) || tombstoned(asset);
                let scoped = |record: Option<&Value>| {
                    record.is_none_or(|record| {
                        record
                            .get("zoneID")
                            .is_none_or(|zone| lookup_zone_matches(zone, self.zone_id.as_ref()))
                            && record
                                .pointer("/fields/masterRef/value/zoneID")
                                .is_none_or(|zone| lookup_zone_matches(zone, self.zone_id.as_ref()))
                    })
                };
                let pair_matches = asset_deleted
                    || asset.is_none_or(|record| {
                        record
                            .pointer("/fields/masterRef/value/recordName")
                            .and_then(Value::as_str)
                            == Some(request.master_record_name.as_str())
                    });
                let resolution = if !scoped(master) || !scoped(asset) || !pair_matches {
                    RecordResolution::Unknown
                } else if primary_deleted || asset_deleted {
                    RecordResolution::Deleted {
                        deleted_at,
                        master_family: primary_deleted
                            && request.target == RecordLookupTarget::Master,
                    }
                } else if master.is_some_and(|record| {
                    record
                        .get("serverErrorCode")
                        .is_some_and(|code| !code.is_null())
                }) || asset.is_some_and(|record| {
                    record
                        .get("serverErrorCode")
                        .is_some_and(|code| !code.is_null())
                }) {
                    RecordResolution::Unknown
                } else if let Some(master) = master {
                    match (
                        serde_json::from_value::<cloudkit::Record>(master.clone()),
                        asset
                            .map(|asset| serde_json::from_value::<cloudkit::Record>(asset.clone())),
                    ) {
                        (Ok(master), Some(Ok(asset)))
                            if master.record_type == "CPLMaster"
                                && asset.record_type == "CPLAsset" =>
                        {
                            match PhotoAsset::try_from_records(
                                master,
                                &asset,
                                RequiredAssetFields::Downloadable,
                            ) {
                                Ok(mut photo) => {
                                    if request.state_id.as_str()
                                        != request.master_record_name.as_str()
                                    {
                                        photo = photo.with_state_record_name(Arc::from(
                                            request.state_id.as_str(),
                                        ));
                                    }
                                    RecordResolution::Present(photo)
                                }
                                Err(error) => RecordResolution::TransientFailure(
                                    ProviderLookupError::Malformed(error.to_string()),
                                ),
                            }
                        }
                        (Ok(master), None)
                            if request.asset_record_name.is_none()
                                && master.record_type == "CPLMaster" =>
                        {
                            RecordResolution::MasterPresent
                        }
                        (Ok(asset), None)
                            if request.target == RecordLookupTarget::Asset
                                && asset.record_type == "CPLAsset" =>
                        {
                            extract_master_ref(&asset.fields)
                                .filter(|name| !name.trim().is_empty())
                                .map_or_else(
                                    || {
                                        SparseShareEvidence::from_fields(&asset.fields).map_or(
                                            RecordResolution::Unknown,
                                            RecordResolution::SparseShareUnresolved,
                                        )
                                    },
                                    |master_record_name| RecordResolution::AssetPresent {
                                        master_record_name: ProviderRecordId::new(
                                            master_record_name,
                                        ),
                                    },
                                )
                        }
                        _ => RecordResolution::Unknown,
                    }
                } else {
                    RecordResolution::Unknown
                };
                if matches!(resolution, RecordResolution::Unknown) {
                    let (reason, stage) =
                        lookup_rejection(master, asset, request, &self.zone_id, pair_matches);
                    let target = match request.target {
                        RecordLookupTarget::Asset => "child",
                        RecordLookupTarget::Master if request.asset_record_name.is_some() => {
                            "paired"
                        }
                        RecordLookupTarget::Master => "master",
                    };
                    let (supplied_owner, supplied_owner_type) =
                        supplied_owner_shape(master, asset, stage, &self.zone_id);
                    *rejection_diagnostics
                        .entry((target, stage, reason, supplied_owner, supplied_owner_type))
                        .or_insert(0usize) += 1;
                }
                if request.target == RecordLookupTarget::Asset
                    && !matches!(resolution, RecordResolution::Deleted { .. })
                {
                    let diagnostic = asset_identity_diagnostic(master, &self.zone_id);
                    *identity_diagnostics.entry(diagnostic).or_insert(0usize) += 1;
                }
                let outcome = match &resolution {
                    RecordResolution::Present(_) => "present",
                    RecordResolution::AssetPresent { .. } => "asset_present",
                    RecordResolution::MasterPresent => "master_present_unpaired",
                    RecordResolution::Deleted { .. } => "deleted",
                    RecordResolution::SparseShareUnresolved(_) | RecordResolution::Unknown => {
                        "unknown"
                    }
                    RecordResolution::TransientFailure(_) => "transient_failure",
                };
                if matches!(resolution, RecordResolution::Present(_)) {
                    url_observed_at
                        .entry(request.state_id.clone())
                        .or_insert(observed_at);
                }
                crate::metrics::record_targeted_lookup(outcome, 1);
                results.push((request.state_id.clone(), resolution));
            }
        }

        let lookup_zone = match self.zone_id.get("zoneName").and_then(Value::as_str) {
            Some("PrimarySync") => "primary",
            Some(name) if name.starts_with("SharedSync") => "shared",
            _ => "other",
        };
        let expected_owner = match self.zone_id.get("ownerRecordName") {
            None => "absent",
            Some(owner) if owner.as_str() == Some("_defaultOwner") => "private_default",
            Some(_) => "other",
        };
        for ((target, stage, reason, supplied_owner, supplied_owner_type), rejected_requests) in
            rejection_diagnostics
        {
            tracing::info!(target: "kei::icloud::photos::album",
                diagnostic = "exact_lookup_rejection_v1",
                target, stage, reason, expected_owner, lookup_zone, rejected_requests, supplied_owner, supplied_owner_type, owner_provenance = "lookup_request",
                "Targeted identity lookup retained unresolved requests"
            );
        }
        // One line per fixed diagnostic class per lookup, not one per asset.
        for ((diagnostic, reference_zone), count) in identity_diagnostics {
            if diagnostic == "master_reference_present" {
                tracing::info!(target: "kei::icloud::photos::album",
                    diagnostic,
                    reference_zone,
                    lookup_zone,
                    count,
                    "Asset identity lookup returned a master reference"
                );
                continue;
            }
            tracing::warn!(target: "kei::icloud::photos::album",
                diagnostic,
                reference_zone,
                lookup_zone,
                count,
                "Asset identity lookup was inconclusive"
            );
        }

        let mut grouped: Vec<(ProviderRecordId, RecordResolution)> =
            Vec::with_capacity(results.len());
        let mut positions: FxHashMap<ProviderRecordId, usize> = FxHashMap::default();
        for (state_id, resolution) in results {
            if let Some(existing) = positions
                .get(&state_id)
                .and_then(|index| grouped.get_mut(*index))
                .map(|(_, resolution)| resolution)
            {
                merge_record_resolution(existing, resolution);
            } else {
                positions.insert(state_id.clone(), grouped.len());
                grouped.push((state_id, resolution));
            }
        }
        let complete = grouped.iter().all(|(_, resolution)| {
            matches!(
                resolution,
                RecordResolution::Present(_) | RecordResolution::Deleted { .. }
            )
        });

        RecordResolutionBatch {
            results: grouped,
            url_observed_at,
            complete,
            rate_limit_observations,
        }
    }
}

#[cfg(test)]
mod tests;
