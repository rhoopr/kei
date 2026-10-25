//! Bounded observations for the explicit missing-reference diagnostic.
//! This scanner never certifies preservation or releases a sync checkpoint.

mod missing_reference;

use anyhow::Context;
use missing_reference::Observation;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::PhotoAlbum;
use crate::icloud::photos::cloudkit::{ChangesZoneResponse, Record};
use crate::icloud::photos::queries::{build_changes_zone_request, encode_params};
use crate::icloud::photos::session::{self, check_changes_zone_error};

struct RetainedLegacyInventory {
    latest: FxHashMap<String, (Record, usize, Observation)>,
}

#[derive(Clone, Copy)]
struct InventoryBudget {
    pages: usize,
    records: usize,
    retained_bytes: usize,
    page_bytes: usize,
}

/// Only fixed categories and aggregate counts may reach normal logs.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LegacyInventoryDiagnostic {
    pub(crate) reason: &'static str,
    pub(crate) phase: &'static str,
    pub(crate) subreason: &'static str,
    pub(crate) eof_observed: bool,
    pub(crate) family_context: &'static str,
    pub(crate) child_soft_deleted: bool,
    pub(crate) pages: usize,
    pub(crate) records: usize,
    pub(crate) transferred_bytes: usize,
    pub(crate) retained_bytes: usize,
    pub(crate) scope_mismatch_component: &'static str,
    pub(crate) expected_owner: &'static str,
    pub(crate) expected_owner_type: &'static str,
    pub(crate) supplied_owner: &'static str,
    pub(crate) supplied_owner_type: &'static str,
    pub(crate) elapsed_secs: f64,
    pub(crate) page_budget: usize,
    pub(crate) record_budget: usize,
    pub(crate) page_byte_budget: usize,
    pub(crate) retained_byte_budget: usize,
}

#[derive(Debug, thiserror::Error)]
enum InventoryFailureKind {
    #[error("Invalid preservation inventory evidence: {0}")]
    Evidence(&'static str),
    #[error("Preservation inventory cancelled")]
    Cancelled,
    #[error("Preservation inventory provider request failed")]
    Provider,
    #[error("Preservation inventory page budget exceeded")]
    Pages,
    #[error("Preservation inventory record budget exceeded")]
    Records,
    #[error("Preservation inventory response page byte budget exceeded")]
    PageBytes,
    #[error("Preservation inventory retained byte budget exceeded")]
    RetainedBytes,
}

impl InventoryFailureKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Evidence(_) => "invalid_inventory_evidence",
            Self::Cancelled => "cancelled",
            Self::Provider => "provider_request_failed",
            Self::Pages => "page_budget",
            Self::Records => "record_budget",
            Self::PageBytes => "response_page_byte_budget",
            Self::RetainedBytes => "retained_byte_budget",
        }
    }

    fn subreason(&self) -> &'static str {
        match self {
            Self::Evidence(reason) => reason,
            _ => self.as_str(),
        }
    }
}

pub(crate) fn classify_legacy_inventory_error(
    error: &anyhow::Error,
) -> Option<LegacyInventoryDiagnostic> {
    error
        .downcast_ref::<InventoryFailureKind>()
        .map(|kind| LegacyInventoryDiagnostic {
            reason: kind.as_str(),
            subreason: kind.subreason(),
            ..LegacyInventoryDiagnostic::default()
        })
}

// Counting serialization avoids allocating another copy of a potentially large
// response. This is a representation budget, not a measurement of process RSS.
fn serialized_bytes(value: &Value) -> anyhow::Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("inventory byte count overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

fn retained_record_bytes(record: &Record) -> anyhow::Result<usize> {
    // Include both owned identity strings (map key and record), record storage,
    // and the retained field representation. Record/page caps also bound entries.
    let fields_bytes = serialized_bytes(&record.fields)?;
    record
        .record_name
        .len()
        .checked_mul(2)
        .and_then(|size| size.checked_add(record.record_type.len()))
        .and_then(|size| {
            size.checked_add(std::mem::size_of::<(String, Record, usize, Observation)>())
        })
        .and_then(|size| size.checked_add(fields_bytes))
        .context("Preservation inventory byte count overflow")
}

fn compact_record(record: &mut Record, masters: &FxHashSet<String>) {
    let family_child = record.record_type == "CPLAsset"
        && record
            .fields
            .get("masterRef")
            .and_then(|field| field.get("value"))
            .and_then(|reference| reference.get("recordName"))
            .and_then(Value::as_str)
            .is_some_and(|master| masters.contains(master))
        && record
            .fields
            .get("isDeleted")
            .and_then(|field| field.get("value"))
            .and_then(Value::as_i64)
            != Some(1);
    let family_master = record.record_type == "CPLMaster" && masters.contains(&record.record_name);
    if record.deleted == Some(true) || (!family_child && !family_master) {
        if record.record_type == "CPLAsset" && record.deleted != Some(true) {
            // Every latest child identity remains available for the final complete
            // family check. Full metadata/resources are needed only for candidates.
            if let Some(fields) = record.fields.as_object_mut() {
                fields.retain(|key, _| key == "masterRef" || key == "isDeleted");
            }
        } else {
            record.fields = Value::Null;
        }
    }
}

impl PhotoAlbum {
    /// Observation only: no catalogue, family hydration or usable checkpoint proof.
    pub(crate) async fn missing_reference_inventory(&self, cancel: &CancellationToken) -> Value {
        let mut progress = LegacyInventoryDiagnostic::default();
        let masters = FxHashSet::default();
        let result = self
            .scan_legacy_preservation_records(
                &masters,
                cancel,
                InventoryBudget {
                    pages: 1000,
                    records: 200_000,
                    retained_bytes: 256 * 1024 * 1024,
                    page_bytes: 64 * 1024 * 1024,
                },
                &mut progress,
            )
            .await;
        let counts = serde_json::json!({"pages":progress.pages,"records":progress.records,"retained_bytes":progress.retained_bytes});
        let inventory = match result {
            Ok(inventory) => inventory,
            Err(error) => {
                return serde_json::json!({"scan":"incomplete","counts":counts,
                "reason":classify_legacy_inventory_error(&error).map_or("invalid_inventory", |d| d.subreason)});
            }
        };
        let mut groups = std::collections::BTreeMap::<String, usize>::new();
        let mut missing = 0usize;
        let mut selected: Option<&Record> = None;
        for (record, _, observation) in inventory.latest.values() {
            if cancel.is_cancelled() {
                return serde_json::json!({"scan":"incomplete","reason":"cancelled","counts":counts});
            }
            if record.record_type != "CPLAsset"
                || record.deleted == Some(true)
                || record.fields.pointer("/masterRef/value").is_some()
            {
                continue;
            }
            missing += 1;
            if selected.is_none_or(|prior| record.record_name < prior.record_name) {
                selected = Some(record);
            }
            let deletion = match record.fields.get("isDeleted") {
                None => "absent",
                Some(field) => match field.get("value").and_then(Value::as_i64) {
                    Some(0) => "false",
                    Some(1) => "true",
                    _ => "malformed",
                },
            };
            let shape = serde_json::json!({"observation":observation,"soft_deletion":deletion});
            *groups.entry(shape.to_string()).or_default() += 1;
        }
        let omitted_groups = groups.len().saturating_sub(32);
        let omitted_observations: usize = groups.values().skip(32).sum();
        let groups: Vec<Value> = groups.into_iter().take(32).map(|(shape,count)| {
            serde_json::json!({"shape":serde_json::from_str::<Value>(&shape).unwrap_or(Value::Null),"count":count})
        }).collect();
        let lookup = match selected {
            Some(record) => {
                let result = self
                    .lookup_missing_reference(&record.record_name, &masters, cancel)
                    .await;
                serde_json::json!({"selection":"one_lexicographically_first_identity","selected_identities":1,
                    "outcome":result.outcome,"scope":result.scope,"shape":result.shape})
            }
            None => serde_json::json!({"selected_identities":0,"outcome":"not_needed"}),
        };
        serde_json::json!({"scan":"eof_observed","purpose":"observation_only","counts":counts,
            "missing_references":missing,"shown_groups":groups.len(),"omitted_groups":omitted_groups,
            "omitted_observations":omitted_observations,"groups":groups,"lookup":lookup})
    }

    async fn scan_legacy_preservation_records(
        &self,
        masters: &FxHashSet<String>,
        cancel: &CancellationToken,
        budget: InventoryBudget,
        progress: &mut LegacyInventoryDiagnostic,
    ) -> anyhow::Result<RetainedLegacyInventory> {
        let url = format!(
            "{}/changes/zone?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let mut cursor: Option<String> = None;
        let mut seen_cursors = FxHashSet::default();
        let mut latest = FxHashMap::<String, (Record, usize, Observation)>::default();
        for _ in 0..budget.pages {
            progress.phase = "request";
            let body =
                build_changes_zone_request(&self.zone_id, cursor.as_deref(), 200).to_string();
            let response = tokio::select! {
                biased;
                () = cancel.cancelled() => anyhow::bail!(InventoryFailureKind::Cancelled),
                response = session::retry_post(self.session.as_ref(), &url, &body, &[("Content-type", "text/plain")], &self.retry_config) => response.context(InventoryFailureKind::Provider)?,
            };
            progress.pages += 1;
            progress.phase = "response_budget";
            let page_bytes = serialized_bytes(&response)?;
            progress.transferred_bytes = progress
                .transferred_bytes
                .checked_add(page_bytes)
                .context("Preservation inventory byte count overflow")?;
            anyhow::ensure!(
                page_bytes <= budget.page_bytes,
                InventoryFailureKind::PageBytes
            );
            progress.phase = "envelope";
            let zones = response
                .get("zones")
                .and_then(Value::as_array)
                .filter(|zones| zones.len() == 1)
                .context(InventoryFailureKind::Evidence("invalid_zone_envelope"))?;
            let zone = zones
                .first()
                .context(InventoryFailureKind::Evidence("missing_zone_scope"))?;
            let zone_id = zone
                .get("zoneID")
                .context(InventoryFailureKind::Evidence("missing_zone_scope"))?;
            progress.phase = "scope";
            (progress.supplied_owner, progress.supplied_owner_type) =
                super::super::owner_diagnostic_shape(zone_id.get("ownerRecordName"));
            progress.scope_mismatch_component =
                if zone_id.get("zoneName") != self.zone_id.get("zoneName") {
                    "zone_name"
                } else if self
                    .zone_id
                    .get("ownerRecordName")
                    .is_some_and(|owner| zone_id.get("ownerRecordName") != Some(owner))
                {
                    "owner"
                } else {
                    "not_applicable"
                };
            anyhow::ensure!(
                zone_id.get("zoneName") == self.zone_id.get("zoneName")
                    && self
                        .zone_id
                        .get("ownerRecordName")
                        .is_none_or(|owner| zone_id.get("ownerRecordName") == Some(owner)),
                InventoryFailureKind::Evidence("zone_scope_mismatch")
            );
            progress.supplied_owner = "not_recorded";
            progress.supplied_owner_type = "not_recorded";
            progress.scope_mismatch_component = "not_applicable";
            progress.phase = "envelope";
            anyhow::ensure!(
                zone.get("moreComing").and_then(Value::as_bool).is_some(),
                InventoryFailureKind::Evidence("completion_marker_missing")
            );
            // Observation only. EOF does not authorize this inventory or a checkpoint.
            progress.eof_observed = zone.get("moreComing").and_then(Value::as_bool) == Some(false);
            let records = zone
                .get("records")
                .and_then(Value::as_array)
                .context(InventoryFailureKind::Evidence("records_array_missing"))?;
            progress.records = progress
                .records
                .checked_add(records.len())
                .context("Preservation inventory budget exceeded")?;
            anyhow::ensure!(
                progress.records <= budget.records,
                InventoryFailureKind::Records
            );
            progress.phase = "record_envelope";
            for record in records {
                anyhow::ensure!(
                    record.get("deleted").and_then(Value::as_bool) == Some(true)
                        || record
                            .get("recordType")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| !kind.trim().is_empty()),
                    InventoryFailureKind::Evidence("record_type_missing")
                );
                anyhow::ensure!(
                    record.get("serverErrorCode").is_none_or(Value::is_null),
                    InventoryFailureKind::Evidence("record_provider_error")
                );
                anyhow::ensure!(
                    record
                        .get("recordName")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.trim().is_empty()),
                    InventoryFailureKind::Evidence("record_identity_missing")
                );
            }
            progress.phase = "decode";
            let response: ChangesZoneResponse = serde_json::from_value(response)?;
            let zone = response
                .zones
                .into_iter()
                .next()
                .context(InventoryFailureKind::Evidence("missing_zone_scope"))?;
            progress.phase = "provider_status";
            check_changes_zone_error(
                zone.server_error_code.as_deref(),
                zone.reason.as_deref(),
                &zone.zone_id.zone_name,
            )
            .context(InventoryFailureKind::Evidence("zone_provider_error"))?;
            progress.phase = "cursor";
            anyhow::ensure!(
                !zone.sync_token.trim().is_empty(),
                InventoryFailureKind::Evidence("cursor_missing")
            );
            anyhow::ensure!(
                seen_cursors.insert(zone.sync_token.clone()),
                InventoryFailureKind::Evidence("cursor_repeated")
            );
            progress.phase = "retention";
            let cursor_bytes = zone
                .sync_token
                .len()
                .checked_mul(2)
                .and_then(|size| size.checked_add(std::mem::size_of::<String>()))
                .context("Preservation inventory byte count overflow")?;
            // Pagination identities remain retained too. Charging both copies
            // conservatively bounds the seen-cursor set and current cursor.
            progress.retained_bytes = progress
                .retained_bytes
                .checked_add(cursor_bytes)
                .context("Preservation inventory byte count overflow")?;
            anyhow::ensure!(
                progress.retained_bytes <= budget.retained_bytes,
                InventoryFailureKind::RetainedBytes
            );
            for mut record in zone.records {
                // Keep fixed pre-compaction categories with every latest record,
                // charged to the same byte budget as the record itself. EOF may
                // select any child, regardless of arrival order or identity length.
                let observation = if record.record_type == "CPLAsset"
                    && record.deleted != Some(true)
                    && record.fields.pointer("/masterRef/value").is_none()
                {
                    Observation::new(
                        &record,
                        latest.get(&record.record_name).map(|(prior, _, _)| prior),
                    )
                } else {
                    Observation::unavailable()
                };
                compact_record(&mut record, masters);
                let bytes = retained_record_bytes(&record)?;
                // Replace, never accumulate versions of the same identity. Later
                // updates and tombstones also replace any earlier full family record.
                if let Some((_, previous_bytes, _)) = latest.remove(&record.record_name) {
                    progress.retained_bytes -= previous_bytes;
                }
                progress.retained_bytes = progress
                    .retained_bytes
                    .checked_add(bytes)
                    .context("Preservation inventory byte count overflow")?;
                anyhow::ensure!(
                    progress.retained_bytes <= budget.retained_bytes,
                    InventoryFailureKind::RetainedBytes
                );
                latest.insert(record.record_name.clone(), (record, bytes, observation));
            }
            anyhow::ensure!(!cancel.is_cancelled(), InventoryFailureKind::Cancelled);
            cursor = Some(zone.sync_token);
            if zone.more_coming {
                continue;
            }
            return Ok(RetainedLegacyInventory { latest });
        }
        progress.phase = "pagination";
        anyhow::bail!(InventoryFailureKind::Pages)
    }
}

#[cfg(test)]
mod tests {
    use super::{InventoryBudget, PhotoAlbum};
    use crate::icloud::photos::album::test_support::{
        canned_changes_page, changes_asset, changes_master, make_album_with_session,
    };
    use crate::test_helpers::MockPhotosSession;
    use rustc_hash::FxHashSet;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    pub(super) const BUDGET: InventoryBudget = InventoryBudget {
        pages: 5,
        records: 100,
        retained_bytes: 8192,
        page_bytes: 64 * 1024 * 1024,
    };
    fn album(pages: Vec<Value>) -> PhotoAlbum {
        let mut session = MockPhotosSession::new();
        for page in pages {
            session = session.ok(page);
        }
        make_album_with_session(100, Box::new(session))
    }
    pub(super) fn masters() -> FxHashSet<String> {
        FxHashSet::from_iter(["master".to_owned()])
    }
    fn records() -> Vec<Value> {
        vec![
            changes_master("master"),
            changes_asset("first", "master"),
            changes_asset("second", "master"),
        ]
    }

    #[tokio::test]
    async fn diagnostic_scanner_bounds_pages_records_and_bytes() {
        let page = canned_changes_page(&records(), "final", false);
        let mut progress = super::LegacyInventoryDiagnostic::default();
        album(vec![page.clone()])
            .scan_legacy_preservation_records(
                &masters(),
                &CancellationToken::new(),
                BUDGET,
                &mut progress,
            )
            .await
            .unwrap();
        for budget in [
            InventoryBudget { pages: 0, ..BUDGET },
            InventoryBudget {
                records: 2,
                ..BUDGET
            },
            InventoryBudget {
                page_bytes: page.to_string().len() - 1,
                ..BUDGET
            },
            InventoryBudget {
                retained_bytes: progress.retained_bytes - 1,
                ..BUDGET
            },
        ] {
            assert!(
                album(vec![page.clone()])
                    .scan_legacy_preservation_records(
                        &masters(),
                        &CancellationToken::new(),
                        budget,
                        &mut super::LegacyInventoryDiagnostic::default()
                    )
                    .await
                    .is_err()
            );
        }
        album(vec![page.clone()])
            .scan_legacy_preservation_records(
                &masters(),
                &CancellationToken::new(),
                InventoryBudget {
                    page_bytes: page.to_string().len(),
                    retained_bytes: progress.retained_bytes,
                    ..BUDGET
                },
                &mut super::LegacyInventoryDiagnostic::default(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn diagnostic_scanner_rejects_wrong_scope_and_repeated_cursor() {
        for field in ["zoneName", "ownerRecordName"] {
            let mut page = canned_changes_page(&records(), "final", false);
            page["zones"][0]["zoneID"][field] = json!("private-other");
            assert!(
                album(vec![page])
                    .scan_legacy_preservation_records(
                        &masters(),
                        &CancellationToken::new(),
                        BUDGET,
                        &mut super::LegacyInventoryDiagnostic::default()
                    )
                    .await
                    .is_err()
            );
        }
        let page = canned_changes_page(&records(), "same", true);
        assert!(
            album(vec![page.clone(), page])
                .scan_legacy_preservation_records(
                    &masters(),
                    &CancellationToken::new(),
                    BUDGET,
                    &mut super::LegacyInventoryDiagnostic::default()
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn diagnostic_inventory_redacts_provider_failures() {
        let session = MockPhotosSession::new()
            .err("private-record https://secret.invalid/?token=private-token");
        let report = make_album_with_session(100, Box::new(session))
            .missing_reference_inventory(&CancellationToken::new())
            .await;
        assert_eq!(report["scan"], "incomplete");
        assert_eq!(report["reason"], "provider_request_failed");
        let output = report.to_string();
        for secret in ["private-record", "secret.invalid", "private-token"] {
            assert!(!output.contains(secret));
        }
    }

    #[tokio::test]
    async fn missing_reference_inventory_groups_every_shape_and_reports_omissions() {
        let mut records = Vec::new();
        for (marker_index, marker) in [
            None,
            Some(json!(true)),
            Some(json!(false)),
            Some(json!("private-marker")),
        ]
        .into_iter()
        .enumerate()
        {
            for linkage in [0, 1, 3] {
                for prior in 0..3 {
                    let name = format!("private-{marker_index}-{linkage}-{prior}");
                    let mut record = changes_asset(&name, "master");
                    record["fields"]
                        .as_object_mut()
                        .unwrap()
                        .remove("masterRef");
                    if prior > 0 {
                        records.push(if prior == 1 {
                            changes_asset(&name, "master")
                        } else {
                            record.clone()
                        });
                    }
                    if let Some(marker) = &marker {
                        record["fields"]["isSparsePrivateRecord"] = json!({"value":marker});
                    }
                    for key in [
                        "linkedShareRecordName",
                        "linkedShareZoneName",
                        "linkedShareZoneOwner",
                    ]
                    .iter()
                    .take(linkage)
                    {
                        record["fields"][key] = json!({"value":"private-link"});
                    }
                    records.push(record);
                }
            }
        }
        let report = album(vec![canned_changes_page(&records, "private-cursor", false)])
            .missing_reference_inventory(&CancellationToken::new())
            .await;
        assert_eq!(report["scan"], "eof_observed");
        assert_eq!(report["missing_references"], 36);
        assert_eq!(report["shown_groups"], 32);
        assert_eq!(report["omitted_groups"], 4);
        assert_eq!(report["omitted_observations"], 4);
        let output = report.to_string();
        assert!(!output.contains("private-"));
        assert!(!output.contains("family"));
        assert!(output.len() < 64 * 1024);
    }

    #[tokio::test]
    async fn missing_reference_inventory_partial_or_cancelled_never_reports_eof() {
        let partial = album(vec![canned_changes_page(&[], "private-cursor", true)])
            .missing_reference_inventory(&CancellationToken::new())
            .await;
        assert_eq!(partial["scan"], "incomplete");
        assert!(partial.get("groups").is_none());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = album(vec![]).missing_reference_inventory(&cancel).await;
        assert_eq!(cancelled["scan"], "incomplete");
        assert!(cancelled.get("lookup").is_none());
    }
}
