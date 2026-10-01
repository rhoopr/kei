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
        anyhow::ensure!(!masters.is_empty(), "Missing preservation scope");
        let url = format!(
            "{}/changes/zone?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let mut cursor: Option<String> = None;
        let mut seen_cursors = FxHashSet::default();
        let mut latest = FxHashMap::<String, Record>::default();
        let mut observed_records = 0usize;
        let mut observed_bytes = 0usize;
        for _ in 0..budget.pages {
            let body =
                build_changes_zone_request(&self.zone_id, cursor.as_deref(), 200).to_string();
            let response = tokio::select! {
                biased;
                () = cancel.cancelled() => anyhow::bail!("Preservation inventory cancelled"),
                response = session::retry_post(self.session.as_ref(), &url, &body, &[("Content-type", "text/plain")], &self.retry_config) => response?,
            };
            observed_bytes = observed_bytes
                .checked_add(response.to_string().len())
                .context("Preservation inventory budget exceeded")?;
            anyhow::ensure!(
                observed_bytes <= 256 * 1024 * 1024,
                "Preservation inventory byte budget exceeded"
            );
            let zones = response
                .get("zones")
                .and_then(Value::as_array)
                .filter(|zones| zones.len() == 1)
                .context("Invalid preservation inventory zones")?;
            let zone = zones
                .first()
                .context("Missing preservation inventory zone")?;
            let zone_id = zone
                .get("zoneID")
                .context("Missing preservation inventory zone")?;
            anyhow::ensure!(
                zone_id.get("zoneName") == self.zone_id.get("zoneName")
                    && self
                        .zone_id
                        .get("ownerRecordName")
                        .is_none_or(|owner| zone_id.get("ownerRecordName") == Some(owner)),
                "Preservation inventory scope mismatch"
            );
            anyhow::ensure!(
                zone.get("moreComing").and_then(Value::as_bool).is_some(),
                "Missing preservation inventory completion marker"
            );
            let records = zone
                .get("records")
                .and_then(Value::as_array)
                .context("Missing preservation inventory records")?;
            observed_records = observed_records
                .checked_add(records.len())
                .context("Preservation inventory budget exceeded")?;
            anyhow::ensure!(
                observed_records <= budget.records,
                "Preservation inventory budget exceeded"
            );
            for record in records {
                anyhow::ensure!(
                    record.get("deleted").and_then(Value::as_bool) == Some(true)
                        || record
                            .get("recordType")
                            .and_then(Value::as_str)
                            .is_some_and(|kind| !kind.trim().is_empty()),
                    "Missing preservation record type"
                );
                anyhow::ensure!(
                    record.get("serverErrorCode").is_none_or(Value::is_null),
                    "Preservation inventory record error"
                );
                anyhow::ensure!(
                    record
                        .get("recordName")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.trim().is_empty()),
                    "Missing preservation record identity"
                );
            }
            let response: ChangesZoneResponse = serde_json::from_value(response)?;
            let zone = response
                .zones
                .into_iter()
                .next()
                .context("Missing preservation inventory zone")?;
            check_changes_zone_error(
                zone.server_error_code.as_deref(),
                zone.reason.as_deref(),
                &zone.zone_id.zone_name,
            )?;
            anyhow::ensure!(
                !zone.sync_token.trim().is_empty(),
                "Missing preservation inventory cursor"
            );
            anyhow::ensure!(
                seen_cursors.insert(zone.sync_token.clone()),
                "Non-progressing preservation inventory"
            );
            for record in zone.records {
                // Later changes and hard tombstones replace earlier observations.
                latest.insert(record.record_name.clone(), record);
            }
            anyhow::ensure!(!cancel.is_cancelled(), "Preservation inventory cancelled");
            cursor = Some(zone.sync_token);
            if zone.more_coming {
                continue;
            }
            let mut expected = FxHashSet::default();
            for record in latest
                .values()
                .filter(|record| record.record_type == "CPLAsset" && record.deleted != Some(true))
            {
                let reference = record
                    .fields
                    .get("masterRef")
                    .and_then(|field| field.get("value"))
                    .context("Unsupported preservation identity")?;
                let master = reference
                    .get("recordName")
                    .and_then(Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                    .context("Unsupported preservation identity")?;
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
            let mut buffer = DeltaRecordBuffer::new();
            let mut events = buffer.process_records(latest.into_values().collect());
            events.extend(buffer.flush());
            let mut children = Vec::new();
            for event in events {
                if !matches!(event.reason, ChangeReason::Created | ChangeReason::Hidden) {
                    continue;
                }
                if let Some(asset) = event.asset
                    && masters.contains(asset.id())
                {
                    anyhow::ensure!(
                        asset.asset_date_evidence().is_some(),
                        "Missing preservation capture date"
                    );
                    anyhow::ensure!(
                        expected.remove(asset.asset_record_name()),
                        "Unexpected preservation child"
                    );
                    children.push(asset.with_source_zone(Arc::from(self.zone_name())));
                }
            }
            anyhow::ensure!(
                expected.is_empty(),
                "Incomplete preservation family hydration"
            );
            children.sort_by(|a, b| a.asset_record_name().cmp(b.asset_record_name()));
            return Ok(CompleteLegacyInventory {
                library: self.zone_name().to_owned(),
                cursor: cursor.context("Missing preservation inventory cursor")?,
                children,
            });
        }
        anyhow::bail!("Preservation inventory page budget exceeded")
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
                        records: 100
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
                        records: 2
                    }
                )
                .await
                .is_err()
        );
    }
}
