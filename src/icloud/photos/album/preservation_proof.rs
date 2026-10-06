//! A complete provider inventory is a prerequisite
//! for legacy preservation, never permission to advance a checkpoint by itself.

use anyhow::Context;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::PhotoAlbum;
use crate::icloud::photos::asset::{DeltaRecordBuffer, PhotoAsset};
use crate::icloud::photos::cloudkit::{ChangesZoneResponse, Record};
use crate::icloud::photos::queries::{build_changes_zone_request, encode_params};
use crate::icloud::photos::session::{self, check_changes_zone_error};
use crate::types::ChangeReason;

#[derive(Debug)]
pub(crate) struct CompleteLegacyInventory {
    pub(crate) library: String,
    pub(crate) cursor: String,
    pub(crate) children: Vec<PhotoAsset>,
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
}

#[derive(Debug, thiserror::Error)]
#[error("Legacy preservation inventory failed: {diagnostic:?}")]
struct LegacyInventoryFailure {
    diagnostic: LegacyInventoryDiagnostic,
    #[source]
    source: anyhow::Error,
}

#[derive(Debug, thiserror::Error)]
enum InventoryFailureKind {
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
            Self::Cancelled => "cancelled",
            Self::Provider => "provider_request_failed",
            Self::Pages => "page_budget",
            Self::Records => "record_budget",
            Self::PageBytes => "response_page_byte_budget",
            Self::RetainedBytes => "retained_byte_budget",
        }
    }
}

pub(crate) fn classify_legacy_inventory_error(
    error: &anyhow::Error,
) -> Option<LegacyInventoryDiagnostic> {
    if let Some(failure) = error.downcast_ref::<LegacyInventoryFailure>() {
        return Some(failure.diagnostic);
    }
    error
        .downcast_ref::<InventoryFailureKind>()
        .map(|kind| LegacyInventoryDiagnostic {
            reason: kind.as_str(),
            subreason: kind.as_str(),
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
        .and_then(|size| size.checked_add(std::mem::size_of::<(String, Record, usize)>()))
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
    /// Bounded, unfiltered zone inventory. Budget exhaustion retains the old hold.
    /// This is separate from display/filter enumeration and never commits a token.
    pub(crate) async fn complete_legacy_inventory(
        &self,
        masters: &FxHashSet<String>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<CompleteLegacyInventory> {
        self.legacy_preservation_inventory(
            masters,
            cancel,
            InventoryBudget {
                pages: 10_000,
                records: 1_000_000,
                retained_bytes: 256 * 1024 * 1024,
                page_bytes: 64 * 1024 * 1024,
            },
        )
        .await
    }

    async fn legacy_preservation_inventory(
        &self,
        masters: &FxHashSet<String>,
        cancel: &CancellationToken,
        budget: InventoryBudget,
    ) -> anyhow::Result<CompleteLegacyInventory> {
        let mut progress = LegacyInventoryDiagnostic {
            phase: "initialization",
            subreason: "missing_candidate_scope",
            family_context: "not_applicable",
            ..LegacyInventoryDiagnostic::default()
        };
        self.scan_legacy_preservation_inventory(masters, cancel, budget, &mut progress)
            .await
            .map_err(|source: anyhow::Error| {
                if let Some(diagnostic) = classify_legacy_inventory_error(&source) {
                    progress.reason = diagnostic.reason;
                    progress.subreason = diagnostic.subreason;
                } else {
                    progress.reason = "invalid_inventory_evidence";
                }
                LegacyInventoryFailure {
                    diagnostic: progress,
                    source,
                }
                .into()
            })
    }

    async fn scan_legacy_preservation_inventory(
        &self,
        masters: &FxHashSet<String>,
        cancel: &CancellationToken,
        budget: InventoryBudget,
        progress: &mut LegacyInventoryDiagnostic,
    ) -> anyhow::Result<CompleteLegacyInventory> {
        anyhow::ensure!(!masters.is_empty(), "Missing preservation scope");
        let url = format!(
            "{}/changes/zone?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let mut cursor: Option<String> = None;
        let mut seen_cursors = FxHashSet::default();
        let mut latest = FxHashMap::<String, (Record, usize)>::default();
        for _ in 0..budget.pages {
            progress.phase = "request";
            progress.subreason = "unclassified";
            let body =
                build_changes_zone_request(&self.zone_id, cursor.as_deref(), 200).to_string();
            let response = tokio::select! {
                biased;
                () = cancel.cancelled() => anyhow::bail!(InventoryFailureKind::Cancelled),
                response = session::retry_post(self.session.as_ref(), &url, &body, &[("Content-type", "text/plain")], &self.retry_config) => response.context(InventoryFailureKind::Provider)?,
            };
            progress.pages += 1;
            progress.phase = "response_budget";
            progress.subreason = "unclassified";
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
            progress.subreason = "invalid_zone_envelope";
            let zones = response
                .get("zones")
                .and_then(Value::as_array)
                .filter(|zones| zones.len() == 1)
                .context("Invalid preservation inventory zones")?;
            progress.subreason = "missing_zone_scope";
            let zone = zones
                .first()
                .context("Missing preservation inventory zone")?;
            progress.subreason = "missing_zone_scope";
            let zone_id = zone
                .get("zoneID")
                .context("Missing preservation inventory zone")?;
            progress.phase = "scope";
            progress.subreason = "zone_scope_mismatch";
            anyhow::ensure!(
                zone_id.get("zoneName") == self.zone_id.get("zoneName")
                    && self
                        .zone_id
                        .get("ownerRecordName")
                        .is_none_or(|owner| zone_id.get("ownerRecordName") == Some(owner)),
                "Preservation inventory scope mismatch"
            );
            progress.phase = "envelope";
            progress.subreason = "completion_marker_missing";
            anyhow::ensure!(
                zone.get("moreComing").and_then(Value::as_bool).is_some(),
                "Missing preservation inventory completion marker"
            );
            progress.subreason = "records_array_missing";
            // Observation only. A terminal marker is not proof of a valid inventory.
            progress.eof_observed = zone.get("moreComing").and_then(Value::as_bool) == Some(false);
            let records = zone
                .get("records")
                .and_then(Value::as_array)
                .context("Missing preservation inventory records")?;
            progress.subreason = "unclassified";
            progress.records = progress
                .records
                .checked_add(records.len())
                .context("Preservation inventory budget exceeded")?;
            anyhow::ensure!(
                progress.records <= budget.records,
                InventoryFailureKind::Records
            );
            for record in records {
                progress.phase = "record_envelope";
                progress.subreason = "record_type_missing";
                anyhow::ensure!(
                    record.get("deleted").and_then(Value::as_bool) == Some(true)
                        || record
                            .get("recordType")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| !kind.trim().is_empty()),
                    "Missing preservation record type"
                );
                progress.subreason = "record_provider_error";
                anyhow::ensure!(
                    record.get("serverErrorCode").is_none_or(Value::is_null),
                    "Preservation inventory record error"
                );
                progress.subreason = "record_identity_missing";
                anyhow::ensure!(
                    record
                        .get("recordName")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.trim().is_empty()),
                    "Missing preservation record identity"
                );
            }
            progress.phase = "decode";
            progress.subreason = "response_decode_failed";
            let response: ChangesZoneResponse = serde_json::from_value(response)?;
            progress.subreason = "missing_zone_scope";
            let zone = response
                .zones
                .into_iter()
                .next()
                .context("Missing preservation inventory zone")?;
            progress.phase = "provider_status";
            progress.subreason = "zone_provider_error";
            check_changes_zone_error(
                zone.server_error_code.as_deref(),
                zone.reason.as_deref(),
                &zone.zone_id.zone_name,
            )?;
            progress.phase = "cursor";
            progress.subreason = "cursor_missing";
            anyhow::ensure!(
                !zone.sync_token.trim().is_empty(),
                "Missing preservation inventory cursor"
            );
            progress.subreason = "cursor_repeated";
            anyhow::ensure!(
                seen_cursors.insert(zone.sync_token.clone()),
                "Non-progressing preservation inventory"
            );
            progress.phase = "retention";
            progress.subreason = "unclassified";
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
                compact_record(&mut record, masters);
                let bytes = retained_record_bytes(&record)?;
                // Replace, never accumulate versions of the same identity. Later
                // updates and tombstones also replace any earlier full family record.
                if let Some((_, previous_bytes)) = latest.remove(&record.record_name) {
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
                latest.insert(record.record_name.clone(), (record, bytes));
            }
            anyhow::ensure!(!cancel.is_cancelled(), InventoryFailureKind::Cancelled);
            cursor = Some(zone.sync_token);
            if zone.more_coming {
                continue;
            }
            let mut expected = FxHashSet::default();
            for record in latest
                .values()
                .map(|(record, _)| record)
                .filter(|record| record.record_type == "CPLAsset" && record.deleted != Some(true))
            {
                progress.phase = "child_reference";
                progress.subreason = "child_master_reference_missing";
                progress.family_context = "unknown";
                progress.child_soft_deleted = record
                    .fields
                    .get("isDeleted")
                    .and_then(|field| field.get("value"))
                    .and_then(Value::as_i64)
                    == Some(1);
                let reference = record
                    .fields
                    .get("masterRef")
                    .and_then(|field| field.get("value"))
                    .context("Unsupported preservation identity")?;
                progress.subreason = "child_master_identity_invalid";
                let master = reference
                    .get("recordName")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .context("Unsupported preservation identity")?;
                progress.subreason = "child_reference_scope_mismatch";
                progress.family_context = if masters.contains(master) {
                    "candidate"
                } else {
                    "unrelated"
                };
                anyhow::ensure!(
                    reference.get("zoneID").is_none_or(|zone| {
                        zone.get("zoneName")
                            .is_some_and(|name| name.as_str() == Some(self.zone_name()))
                            && zone.get("ownerRecordName").is_none_or(|owner| {
                                self.zone_id.get("ownerRecordName") == Some(owner)
                            })
                    }),
                    "Cross-library preservation reference"
                );
                if masters.contains(master)
                    && record
                        .fields
                        .get("isDeleted")
                        .and_then(|field| field.get("value"))
                        .and_then(Value::as_i64)
                        != Some(1)
                {
                    expected.insert(record.record_name.clone());
                }
            }
            // Compact unrelated records prove completeness only. Hydration must
            // consume complete candidate-family records, never their projections.
            progress.phase = "family_hydration";
            progress.subreason = "unclassified";
            progress.family_context = "candidate";
            progress.child_soft_deleted = false;
            let family_records = latest
                .into_values()
                .map(|(record, _)| record)
                .filter(|record| {
                    (record.record_type == "CPLMaster" && masters.contains(&record.record_name))
                        || (record.record_type == "CPLAsset"
                            && expected.contains(&record.record_name))
                })
                .collect();
            let mut buffer = DeltaRecordBuffer::new();
            let mut events = buffer.process_records(family_records);
            events.extend(buffer.flush());
            let mut children = Vec::new();
            for event in events {
                if !matches!(event.reason, ChangeReason::Created | ChangeReason::Hidden) {
                    continue;
                }
                if let Some(asset) = event.asset
                    && masters.contains(asset.id())
                {
                    progress.phase = "capture_date";
                    progress.subreason = "capture_date_missing";
                    anyhow::ensure!(
                        asset.asset_date_evidence().is_some(),
                        "Missing preservation capture date"
                    );
                    progress.phase = "family_hydration";
                    progress.subreason = "unexpected_child";
                    anyhow::ensure!(
                        expected.remove(asset.asset_record_name()),
                        "Unexpected preservation child"
                    );
                    children.push(asset.with_source_zone(Arc::from(self.zone_name())));
                }
            }
            progress.phase = "family_hydration";
            progress.subreason = "family_hydration_incomplete";
            anyhow::ensure!(
                expected.is_empty(),
                "Incomplete preservation family hydration"
            );
            children.sort_by(|a, b| a.asset_record_name().cmp(b.asset_record_name()));
            progress.subreason = "cursor_missing";
            return Ok(CompleteLegacyInventory {
                library: self.zone_name().to_owned(),
                cursor: cursor.context("Missing preservation inventory cursor")?,
                children,
            });
        }
        progress.phase = "pagination";
        progress.subreason = "page_budget";
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

    const BUDGET: InventoryBudget = InventoryBudget {
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
    fn masters() -> FxHashSet<String> {
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
    async fn preservation_inventory_includes_hidden_late_siblings_and_final_cursor() {
        let mut hidden = changes_asset("third", "master");
        hidden["fields"]["isHidden"] = json!({"value":1});
        let result = album(vec![
            canned_changes_page(&records(), "page1", true),
            canned_changes_page(&[hidden], "final", false),
        ])
        .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
        .await
        .unwrap();
        assert_eq!(result.library, "PrimarySync");
        assert_eq!(result.cursor, "final");
        assert_eq!(
            result
                .children
                .iter()
                .map(|a| a.asset_record_name())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert!(result.children[2].metadata().is_hidden);
    }

    #[tokio::test]
    async fn preservation_inventory_folds_later_tombstones_and_metadata() {
        for hard in [false, true] {
            let mut deleted = changes_asset("first", "master");
            if hard {
                deleted = json!({"recordName":"first","deleted":true});
            } else {
                deleted["fields"]["isDeleted"] = json!({"value":1});
            }
            let mut changed = changes_asset("second", "master");
            changed["fields"]["isFavorite"] = json!({"value":1});
            let result = album(vec![
                canned_changes_page(&records(), "page1", true),
                canned_changes_page(&[deleted, changed], "final", false),
            ])
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
            .await
            .unwrap();
            assert_eq!(result.children.len(), 1);
            assert_eq!(result.children[0].asset_record_name(), "second");
            assert!(result.children[0].metadata().is_favorite);
        }
    }

    #[tokio::test]
    async fn preservation_inventory_rejects_partial_malformed_and_wrong_scope() {
        let valid = canned_changes_page(&records(), "final", false);
        let mut invalids = Vec::new();
        for field in ["records", "syncToken", "moreComing", "zoneID"] {
            let mut page = valid.clone();
            page["zones"][0].as_object_mut().unwrap().remove(field);
            invalids.push(page);
        }
        let mut page = valid.clone();
        page["zones"][0]["zoneID"]["zoneName"] = json!("SharedSync-other");
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]
            .as_object_mut()
            .unwrap()
            .remove("moreComing");
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]["syncToken"] = json!(" ");
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]["records"][1]
            .as_object_mut()
            .unwrap()
            .remove("recordType");
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["recordName"] = json!("");
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["serverErrorCode"] = json!("UNKNOWN_ITEM");
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("masterRef");
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["fields"]["masterRef"]["value"]["zoneID"] =
            json!({"zoneName":"SharedSync-other"});
        invalids.push(page);
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("assetDate");
        invalids.push(page);
        for (i, page) in invalids.into_iter().enumerate() {
            assert!(
                album(vec![page])
                    .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
                    .await
                    .is_err(),
                "invalid case {i}"
            );
        }
    }

    #[tokio::test]
    async fn preservation_inventory_rejects_cancel_budget_and_repeated_cursor() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            album(vec![])
                .legacy_preservation_inventory(&masters(), &cancel, BUDGET)
                .await
                .is_err()
        );
        let page = canned_changes_page(&records(), "same", true);
        assert!(
            album(vec![page.clone(), page.clone()])
                .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
                .await
                .is_err()
        );
        assert!(
            album(vec![page.clone()])
                .legacy_preservation_inventory(
                    &masters(),
                    &CancellationToken::new(),
                    InventoryBudget {
                        pages: 1,
                        records: 100,
                        retained_bytes: BUDGET.retained_bytes,
                        page_bytes: BUDGET.page_bytes,
                    }
                )
                .await
                .is_err()
        );
        assert!(
            album(vec![page])
                .legacy_preservation_inventory(
                    &masters(),
                    &CancellationToken::new(),
                    InventoryBudget {
                        pages: 5,
                        records: 2,
                        retained_bytes: BUDGET.retained_bytes,
                        page_bytes: BUDGET.page_bytes,
                    }
                )
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn preservation_inventory_large_unrelated_payload_keeps_late_hidden_child() {
        let mut unrelated = changes_asset("unrelated", "unrelated-master");
        unrelated["fields"]["opaquePayload"] =
            json!({"value": "x".repeat(BUDGET.retained_bytes * 2)});
        let mut hidden = changes_asset("second", "master");
        hidden["fields"]["isHidden"] = json!({"value": 1});
        let pages = vec![
            canned_changes_page(&[unrelated, changes_master("master")], "page1", true),
            canned_changes_page(&[changes_asset("first", "master"), hidden], "final", false),
        ];
        assert!(
            pages
                .iter()
                .map(|page| page.to_string().len())
                .sum::<usize>()
                > BUDGET.retained_bytes
        );
        let inventory = album(pages)
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
            .await
            .unwrap();
        assert_eq!(inventory.cursor, "final");
        assert_eq!(inventory.children.len(), 2);
        assert_eq!(inventory.children[1].asset_record_name(), "second");
        assert!(inventory.children[1].metadata().is_hidden);
    }
    #[tokio::test]
    async fn preservation_inventory_retained_byte_boundary_is_inclusive() {
        let page = canned_changes_page(&records(), "final", false);
        let mut progress = super::LegacyInventoryDiagnostic::default();
        album(vec![page.clone()])
            .scan_legacy_preservation_inventory(
                &masters(),
                &CancellationToken::new(),
                BUDGET,
                &mut progress,
            )
            .await
            .unwrap();
        assert_eq!(progress.pages, 1);
        assert_eq!(progress.records, 3);
        assert_eq!(progress.transferred_bytes, page.to_string().len());
        let exact = InventoryBudget {
            retained_bytes: progress.retained_bytes,
            ..BUDGET
        };
        album(vec![page.clone()])
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), exact)
            .await
            .unwrap();
        let error = album(vec![page])
            .legacy_preservation_inventory(
                &masters(),
                &CancellationToken::new(),
                InventoryBudget {
                    retained_bytes: exact.retained_bytes - 1,
                    ..BUDGET
                },
            )
            .await
            .unwrap_err();
        let diagnostic = super::classify_legacy_inventory_error(&error).unwrap();
        assert_eq!(diagnostic.reason, "retained_byte_budget");
        assert_eq!(diagnostic.phase, "retention");
        assert_eq!(diagnostic.subreason, "retained_byte_budget");
        assert!(diagnostic.eof_observed);
        assert_eq!(diagnostic.retained_bytes, exact.retained_bytes);
        assert_eq!(diagnostic.pages, 1);
        assert_eq!(diagnostic.records, 3);
    }

    #[tokio::test]
    async fn preservation_inventory_response_page_boundary_is_inclusive() {
        let page = canned_changes_page(&records(), "final", false);
        let bytes = page.to_string().len();
        album(vec![page.clone()])
            .legacy_preservation_inventory(
                &masters(),
                &CancellationToken::new(),
                InventoryBudget {
                    page_bytes: bytes,
                    ..BUDGET
                },
            )
            .await
            .unwrap();
        let error = album(vec![page])
            .legacy_preservation_inventory(
                &masters(),
                &CancellationToken::new(),
                InventoryBudget {
                    page_bytes: bytes - 1,
                    ..BUDGET
                },
            )
            .await
            .unwrap_err();
        let diagnostic = super::classify_legacy_inventory_error(&error).unwrap();
        assert_eq!(diagnostic.reason, "response_page_byte_budget");
        assert_eq!(diagnostic.phase, "response_budget");
        assert_eq!(diagnostic.subreason, "response_page_byte_budget");
        assert!(!diagnostic.eof_observed);
        assert_eq!(diagnostic.transferred_bytes, bytes);
        assert_eq!(diagnostic.retained_bytes, 0);
    }

    #[tokio::test]
    async fn preservation_inventory_replaces_prior_payload_before_charging_budget() {
        let mut pages = vec![canned_changes_page(&records(), "initial", true)];
        for index in 0..3 {
            let mut updated = changes_asset("second", "master");
            updated["fields"]["opaquePayload"] = json!({"value": "x".repeat(3000)});
            updated["fields"]["isFavorite"] = json!({"value": index % 2});
            pages.push(canned_changes_page(
                &[updated],
                &format!("update-{index}"),
                true,
            ));
        }
        let tombstone = json!({"recordName": "first", "deleted": true});
        pages.push(canned_changes_page(&[tombstone], "final", false));
        assert!(
            pages
                .iter()
                .map(|page| page.to_string().len())
                .sum::<usize>()
                > BUDGET.retained_bytes
        );
        let result = album(pages)
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
            .await
            .unwrap();
        assert_eq!(result.children.len(), 1);
        assert_eq!(result.children[0].asset_record_name(), "second");
        assert!(!result.children[0].metadata().is_favorite);
    }

    #[tokio::test]
    async fn preservation_inventory_latest_reference_can_leave_or_join_candidate_family() {
        let mut moved = changes_asset("first", "other-master");
        moved["fields"]["opaquePayload"] = json!({"value": "x".repeat(BUDGET.retained_bytes * 2)});
        let joined = changes_asset("previously-unrelated", "master");
        let mut unrelated = joined.clone();
        unrelated["fields"]["masterRef"] = json!({"value": {"recordName": "other-master"}});
        let result = album(vec![
            canned_changes_page(
                &[
                    changes_master("master"),
                    changes_asset("first", "master"),
                    unrelated,
                ],
                "initial",
                true,
            ),
            canned_changes_page(&[moved, joined], "final", false),
        ])
        .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
        .await
        .unwrap();
        assert_eq!(result.children.len(), 1);
        assert_eq!(
            result.children[0].asset_record_name(),
            "previously-unrelated"
        );
    }

    #[tokio::test]
    async fn preservation_inventory_failure_diagnostic_redacts_provider_error() {
        let session = MockPhotosSession::new()
            .err("private-record https://secret.invalid/?token=private-token");
        let error = make_album_with_session(100, Box::new(session))
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
            .await
            .unwrap_err();
        let diagnostic = super::classify_legacy_inventory_error(&error).unwrap();
        assert_eq!(diagnostic.reason, "provider_request_failed");
        assert_eq!(diagnostic.phase, "request");
        assert_eq!(diagnostic.subreason, "provider_request_failed");
        assert!(!diagnostic.eof_observed);
        let rendered = format!("{diagnostic:?} {error}");
        for private in ["private-record", "secret.invalid", "private-token"] {
            assert!(!rendered.contains(private), "{rendered}");
        }
    }

    // RELEASE_OUTCOME_CONTROLS_START
    fn release_refusal_cases() -> Vec<(Value, &'static str, &'static str, &'static str)> {
        let valid = canned_changes_page(&records(), "final", false);
        let mut cases = Vec::new();
        let mut add = |page, context, phase, subreason| {
            cases.push((page, context, phase, subreason));
        };
        add(
            json!({"zones":[]}),
            "Invalid preservation inventory zones",
            "envelope",
            "invalid_zone_envelope",
        );
        let mut page = valid.clone();
        page["zones"][0].as_object_mut().unwrap().remove("zoneID");
        add(
            page,
            "Missing preservation inventory zone",
            "envelope",
            "missing_zone_scope",
        );
        let mut page = valid.clone();
        page["zones"][0]["zoneID"]["zoneName"] = json!("private-other-zone");
        add(
            page,
            "Preservation inventory scope mismatch",
            "scope",
            "zone_scope_mismatch",
        );
        let mut page = valid.clone();
        page["zones"][0]
            .as_object_mut()
            .unwrap()
            .remove("moreComing");
        add(
            page,
            "Missing preservation inventory completion marker",
            "envelope",
            "completion_marker_missing",
        );
        let mut page = valid.clone();
        page["zones"][0].as_object_mut().unwrap().remove("records");
        add(
            page,
            "Missing preservation inventory records",
            "envelope",
            "records_array_missing",
        );
        let mut page = valid.clone();
        page["zones"][0]["records"][1]
            .as_object_mut()
            .unwrap()
            .remove("recordType");
        add(
            page,
            "Missing preservation record type",
            "record_envelope",
            "record_type_missing",
        );
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["serverErrorCode"] = json!("private-provider-error");
        add(
            page,
            "Preservation inventory record error",
            "record_envelope",
            "record_provider_error",
        );
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["recordName"] = json!("");
        add(
            page,
            "Missing preservation record identity",
            "record_envelope",
            "record_identity_missing",
        );
        let mut page = valid.clone();
        page["zones"][0]["syncToken"] = json!(" ");
        add(
            page,
            "Missing preservation inventory cursor",
            "cursor",
            "cursor_missing",
        );
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("masterRef");
        add(
            page,
            "Unsupported preservation identity",
            "child_reference",
            "child_master_reference_missing",
        );
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["fields"]["masterRef"]["value"]["recordName"] = json!("");
        add(
            page,
            "Unsupported preservation identity",
            "child_reference",
            "child_master_identity_invalid",
        );
        let mut page = valid.clone();
        page["zones"][0]["records"][1]["fields"]["masterRef"]["value"]["zoneID"] =
            json!({"zoneName":"PrimarySync","ownerRecordName":"private-owner"});
        add(
            page,
            "Cross-library preservation reference",
            "child_reference",
            "child_reference_scope_mismatch",
        );
        let mut page = valid;
        page["zones"][0]["records"][1]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("assetDate");
        add(
            page,
            "Incomplete preservation family hydration",
            "family_hydration",
            "family_hydration_incomplete",
        );
        let mut page = canned_changes_page(&records(), "final", false);
        page["zones"][0]["syncToken"] = Value::Null;
        add(
            page,
            "invalid type: null",
            "decode",
            "response_decode_failed",
        );
        let mut page = canned_changes_page(&records(), "final", false);
        page["zones"][0]["serverErrorCode"] = json!("BAD_REQUEST");
        page["zones"][0]["reason"] = json!("private-provider-reason");
        add(
            page,
            "private-provider-reason",
            "provider_status",
            "zone_provider_error",
        );
        let page = canned_changes_page(&[changes_asset("first", "master")], "final", false);
        add(
            page,
            "Incomplete preservation family hydration",
            "family_hydration",
            "family_hydration_incomplete",
        );
        cases
    }

    #[tokio::test]
    async fn release_inventory_outcome_controls_keep_guard_contexts() {
        for (page, context, _, _) in release_refusal_cases() {
            let error = album(vec![page])
                .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
                .await
                .unwrap_err();
            assert!(
                error
                    .chain()
                    .any(|cause| cause.to_string().contains(context)),
                "{context}: {error:#}"
            );
        }
        let result = album(vec![canned_changes_page(&records(), "final", false)])
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
            .await
            .unwrap();
        assert_eq!(result.cursor, "final");
        assert_eq!(
            result
                .children
                .iter()
                .map(|asset| asset.asset_record_name())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[tokio::test]
    async fn release_inventory_outcome_controls_private_reference_matrix() {
        for candidate in [false, true] {
            for soft_deleted in [false, true] {
                for owner in [None, Some("private-owner")] {
                    let mut child = changes_asset(
                        "private-child",
                        if candidate {
                            "master"
                        } else {
                            "private-master"
                        },
                    );
                    child["fields"]["isDeleted"] = json!({"value":i64::from(soft_deleted)});
                    child["fields"]["masterRef"]["value"]["zoneID"] =
                        json!({"zoneName":"PrimarySync"});
                    if let Some(owner) = owner {
                        child["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                            json!(owner);
                    }
                    let pages = vec![
                        canned_changes_page(&[changes_master("master")], "initial", true),
                        canned_changes_page(&[child], "final", false),
                    ];
                    let result = album(pages)
                        .legacy_preservation_inventory(
                            &masters(),
                            &CancellationToken::new(),
                            BUDGET,
                        )
                        .await;
                    if owner.is_some() {
                        let error = result.unwrap_err();
                        assert!(error.chain().any(
                            |cause| cause.to_string() == "Cross-library preservation reference"
                        ));
                    } else {
                        let inventory = result.unwrap();
                        assert_eq!(inventory.cursor, "final");
                        assert_eq!(
                            inventory.children.len(),
                            usize::from(candidate && !soft_deleted)
                        );
                    }
                }
            }
        }
    }
    // RELEASE_OUTCOME_CONTROLS_END

    fn diagnostic_warning(error: &anyhow::Error) -> String {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("warning.log");
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(std::sync::Mutex::new(std::fs::File::create(&log).unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            crate::download::legacy_preservation::log_preservation_hold("preparation", error);
        });
        std::fs::read_to_string(log).unwrap()
    }

    #[tokio::test]
    async fn preservation_inventory_diagnostics_distinguish_generic_refusals_and_redact() {
        for (page, _, phase, subreason) in release_refusal_cases() {
            let error = album(vec![page])
                .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
                .await.unwrap_err()
                .context("private-provider-id https://private.invalid/?token=private-token /private/path");
            let diagnostic = super::classify_legacy_inventory_error(&error).unwrap();
            assert_eq!(diagnostic.reason, "invalid_inventory_evidence");
            assert_eq!(diagnostic.phase, phase);
            assert_eq!(diagnostic.subreason, subreason);
            let log = diagnostic_warning(&error);
            assert!(
                log.contains("diagnostic=\"legacy_inventory_failure_v2\""),
                "{log}"
            );
            assert!(log.contains(&format!("phase=\"{phase}\"")), "{log}");
            assert!(log.contains(&format!("subreason=\"{subreason}\"")), "{log}");
            for private in ["private", "first", "second", "token=", "https://"] {
                assert!(!log.contains(private), "{log}");
            }
        }
    }

    #[tokio::test]
    async fn preservation_inventory_diagnostics_terminal_family_context_is_observation_only() {
        for candidate in [false, true] {
            for soft_deleted in [false, true] {
                let mut child = changes_asset(
                    "private-child",
                    if candidate {
                        "master"
                    } else {
                        "private-master"
                    },
                );
                child["fields"]["isDeleted"] = json!({"value":i64::from(soft_deleted)});
                child["fields"]["masterRef"]["value"]["zoneID"] =
                    json!({"zoneName":"PrimarySync","ownerRecordName":"private-owner"});
                let error = album(vec![
                    canned_changes_page(&[changes_master("master")], "initial", true),
                    canned_changes_page(&[child], "final", false),
                ])
                .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
                .await
                .unwrap_err();
                let diagnostic = super::classify_legacy_inventory_error(&error).unwrap();
                assert_eq!(diagnostic.reason, "invalid_inventory_evidence");
                assert_eq!(diagnostic.phase, "child_reference");
                assert_eq!(diagnostic.subreason, "child_reference_scope_mismatch");
                assert!(diagnostic.eof_observed);
                assert_eq!(
                    diagnostic.family_context,
                    if candidate { "candidate" } else { "unrelated" }
                );
                assert_eq!(diagnostic.child_soft_deleted, soft_deleted);
                assert_eq!(diagnostic.pages, 2);
                assert_eq!(diagnostic.records, 2);
                let log = diagnostic_warning(&error);
                assert!(log.contains("eof_observed=true"));
                assert!(log.contains(&format!("child_soft_deleted={soft_deleted}")));
                assert!(!log.contains("private"), "{log}");
            }
        }
    }

    #[tokio::test]
    async fn preservation_inventory_diagnostics_no_eof_or_missing_reference_never_infers_family() {
        let mut page = canned_changes_page(&records(), "final", false);
        page["zones"][0]
            .as_object_mut()
            .unwrap()
            .remove("moreComing");
        let error = album(vec![page])
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
            .await
            .unwrap_err();
        let diagnostic = super::classify_legacy_inventory_error(&error).unwrap();
        assert!(!diagnostic.eof_observed);
        assert_eq!(diagnostic.family_context, "not_applicable");
        let mut child = changes_asset("private-child", "master");
        child["fields"]["isDeleted"] = json!({"value":1});
        child["fields"].as_object_mut().unwrap().remove("masterRef");
        let error = album(vec![canned_changes_page(&[child], "final", false)])
            .legacy_preservation_inventory(&masters(), &CancellationToken::new(), BUDGET)
            .await
            .unwrap_err();
        let diagnostic = super::classify_legacy_inventory_error(&error).unwrap();
        assert!(diagnostic.eof_observed);
        assert_eq!(diagnostic.family_context, "unknown");
        assert!(diagnostic.child_soft_deleted);
        assert_eq!(diagnostic.subreason, "child_master_reference_missing");
    }
}
