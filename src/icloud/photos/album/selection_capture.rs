//! Original rank-query evidence for a qualified private selection run.

use anyhow::{Context, Result};
use serde_json::Value;

use super::PhotoAlbum;
use crate::icloud::photos::{changes_json, inbox::ShadowCapture};

#[derive(Clone)]
pub(crate) struct RankCapture {
    pub(crate) capture: ShadowCapture,
    pub(crate) generation: String,
    pub(crate) pass_key: String,
}

impl RankCapture {
    pub(crate) async fn capture(&self, request: Value, body: Vec<u8>) -> Result<Value> {
        let response = validated_rank_page(&request, &body)?;
        self.capture
            .db
            .capture_selection_rank_page(
                self.capture.owner.clone(),
                self.generation.clone(),
                self.pass_key.clone(),
                request,
                body,
                crate::state::db::provider_generations::MAX_GENERATION_BYTES,
            )
            .await?;
        Ok(response)
    }
}

fn validate_nested(value: &Value, zone: &Value) -> Result<()> {
    match value {
        Value::Object(fields) => {
            anyhow::ensure!(
                fields.get("serverErrorCode").is_none_or(Value::is_null),
                "Rank source contains a failed record"
            );
            if let Some(scope) = fields.get("zoneID") {
                anyhow::ensure!(
                    scope.as_object().is_some_and(|parts| {
                        parts
                            .get("zoneName")
                            .and_then(Value::as_str)
                            .is_some_and(|name| !name.trim().is_empty())
                            && parts
                                .iter()
                                .all(|(key, value)| zone.get(key) == Some(value))
                    }),
                    "Rank source scope disagrees with its request"
                );
            }
            for member in fields.values() {
                validate_nested(member, zone)?;
            }
        }
        Value::Array(values) => {
            for member in values {
                validate_nested(member, zone)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate the entire original page before typed pairing. EOF and query tokens
/// remain observational telemetry, not snapshot, absence or cursor authority.
pub(crate) fn validated_rank_page(request: &Value, body: &[u8]) -> Result<Value> {
    anyhow::ensure!(
        body.len() <= super::super::inbox::MAX_CHANGES_PAGE_BYTES,
        "Rank page exceeds capture limit"
    );
    let zone = request
        .get("zoneID")
        .context("Missing rank request scope")?;
    anyhow::ensure!(
        zone.get("ownerRecordName").and_then(Value::as_str) == Some("_defaultOwner")
            && zone
                .get("zoneName")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty()),
        "Unsupported rank request owner"
    );
    let response = changes_json::parse(body)?;
    validate_nested(&response, zone)?;
    let records = response
        .get("records")
        .and_then(Value::as_array)
        .context("Missing rank records")?;
    for record in records {
        anyhow::ensure!(
            record
                .get("recordName")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty()),
            "Missing rank source identity"
        );
        let _: crate::icloud::photos::cloudkit::Record = serde_json::from_value(record.clone())
            .map_err(|_invalid| anyhow::anyhow!("Invalid rank source record"))?;
    }
    Ok(response)
}

impl PhotoAlbum {
    /// Independently bound ownership protects old private obligations even if
    /// the current pass is not an eligible activation selector.
    pub(crate) fn owned_private_scope(&self) -> Result<Option<(ShadowCapture, String, Value)>> {
        let Some((capture, database)) = &self.shadow_capture else {
            return Ok(None);
        };
        if database.as_ref() != "private"
            || self.zone_id.get("ownerRecordName").and_then(Value::as_str) != Some("_defaultOwner")
        {
            return Ok(None);
        }
        Ok(Some((
            capture.clone(),
            capture.scope(database, &self.zone_id)?,
            self.zone_id.as_ref().clone(),
        )))
    }

    /// Qualify each member before enabling the whole private pass set. Named
    /// folder predicates are supplied by the provider owner, never invented.
    pub(crate) fn private_selection_scope(&self) -> Result<Option<(ShadowCapture, String, Value)>> {
        let Some(owned) = self.owned_private_scope()? else {
            return Ok(None);
        };
        if !self.cross_zone_sources.is_empty() {
            return Ok(None);
        }
        let supported = match self.container_id.as_deref() {
            None => {
                self.list_type.as_ref() == super::QUERY_ALL_LIST
                    && self.obj_type.as_ref() == super::QUERY_ALL_OBJ
                    && self.query_filter.is_none()
            }
            Some(id) => {
                !id.trim().is_empty()
                    && !self.name.trim().is_empty()
                    && self.list_type.as_ref() == super::QUERY_FOLDER_LIST
                    && self.obj_type.as_ref()
                        == format!("CPLContainerRelationNotDeletedByAssetDate:{id}")
                    && self.query_filter.as_deref()
                        == Some(
                            &serde_json::json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":id}}]),
                        )
            }
        };
        if !supported {
            return Ok(None);
        }
        Ok(Some(owned))
    }

    pub(crate) fn with_rank_capture(mut self, generation: &str, pass_key: &str) -> Self {
        if let Some((capture, _)) = &self.shadow_capture {
            self.rank_capture = Some(RankCapture {
                capture: capture.clone(),
                generation: generation.to_owned(),
                pass_key: pass_key.to_owned(),
            });
        }
        self
    }
}
