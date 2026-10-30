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
    /// A frozen task may refresh only through its original authenticated source.
    /// The selecting album still owns its destination and membership policy.
    pub(crate) fn task_source(&self, zone: &str) -> anyhow::Result<Option<Self>> {
        if self.zone_name() == zone {
            return Ok(Some(self.clone()));
        }
        let scopes = self.selector_sources()?;
        if !scopes.iter().any(|(_, _, identity)| {
            identity.get("zoneName").and_then(serde_json::Value::as_str) == Some(zone)
        }) {
            return Ok(None);
        }
        let mut sources = self
            .cross_zone_sources
            .iter()
            .filter(|source| source.zone_name() == zone);
        let source = sources.next();
        anyhow::ensure!(sources.next().is_none(), "Ambiguous task source");
        Ok(source.cloned())
    }

    /// Reconstruct a supported private query using only this authenticated owner.
    pub(crate) fn recorded_private_selection(
        &self,
        recorded: &Value,
        scope: &str,
        zone: &Value,
    ) -> Result<Option<Self>> {
        let Some((_, actual_scope, actual_zone)) = self.owned_private_scope()? else {
            return Ok(None);
        };
        if actual_scope != scope || &actual_zone != zone {
            return Ok(None);
        }
        let Some(fields) = recorded.as_object() else {
            return Ok(None);
        };
        let keys = [
            "zone",
            "name",
            "list_type",
            "obj_type",
            "query_filter",
            "container_id",
            "cross_zone_sources",
        ];
        if fields.len() != keys.len()
            || keys.iter().any(|key| !fields.contains_key(*key))
            || fields.get("zone") != Some(zone)
            || fields
                .get("cross_zone_sources")
                .and_then(Value::as_array)
                .is_none_or(|items| !items.is_empty())
        {
            return Ok(None);
        }
        let (Some(name), Some(list), Some(object)) = (
            fields.get("name").and_then(Value::as_str),
            fields.get("list_type").and_then(Value::as_str),
            fields.get("obj_type").and_then(Value::as_str),
        ) else {
            return Ok(None);
        };
        let container = match fields.get("container_id") {
            Some(Value::Null) => None,
            Some(Value::String(value)) => Some(std::sync::Arc::from(value.as_str())),
            _ => return Ok(None),
        };
        let filter = match fields.get("query_filter") {
            Some(Value::Null) => None,
            Some(value) => Some(std::sync::Arc::new(value.clone())),
            None => return Ok(None),
        };
        let mut result = self.clone();
        result.name = std::sync::Arc::from(name);
        result.list_type = std::sync::Arc::from(list);
        result.obj_type = std::sync::Arc::from(object);
        result.container_id = container;
        result.query_filter = filter;
        result.cross_zone_sources.clear();
        result.rank_capture = None;
        if result.selection_scope() != *recorded || result.private_selection_scope()?.is_none() {
            return Ok(None);
        }
        Ok(Some(result))
    }

    /// Resolve only authenticated sources already attached by album selection.
    /// Duplicate labels with distinct database/owner identities are ambiguous.
    pub(crate) fn publication_scope(
        &self,
        library: &str,
    ) -> Result<Option<(ShadowCapture, String, Value)>> {
        let sources: Vec<_> = std::iter::once(self)
            .chain(self.cross_zone_sources.iter())
            .filter(|source| source.zone_name() == library)
            .collect();
        // Unowned direct transfer APIs cannot mint scoped receipts. Preserve
        // their existing prepared owner without inventing authentication.
        if sources.iter().all(|source| source.shadow_capture.is_none()) {
            return Ok(None);
        }
        let mut found: Option<(ShadowCapture, String, Value)> = None;
        for source in sources {
            if source.zone_name() != library {
                continue;
            }
            let candidate = source
                .authenticated_publication_scope()?
                .context("Matching publication source lacks authenticated scope")?;
            if let Some(previous) = &found {
                anyhow::ensure!(
                    previous.0.owner == candidate.0.owner
                        && std::sync::Arc::ptr_eq(&previous.0.db, &candidate.0.db)
                        && previous.1 == candidate.1
                        && previous.2 == candidate.2,
                    "Ambiguous authenticated publication source"
                );
            } else {
                found = Some(candidate);
            }
        }
        Ok(found)
    }

    pub(crate) fn selector_sources(&self) -> Result<Vec<(ShadowCapture, String, Value)>> {
        let Some(owner) = self.authenticated_publication_scope()? else {
            return Ok(Vec::new());
        };
        let mut sources = vec![owner.clone()];
        for source in &self.cross_zone_sources {
            let candidate = source
                .authenticated_publication_scope()?
                .context("Selected source lacks authenticated ownership")?;
            anyhow::ensure!(
                owner.0.owner == candidate.0.owner
                    && std::sync::Arc::ptr_eq(&owner.0.db, &candidate.0.db),
                "Selected source account mismatch"
            );
            anyhow::ensure!(
                sources.iter().all(|existing| existing.2.get("zoneName")
                    != candidate.2.get("zoneName")
                    || (existing.1 == candidate.1 && existing.2 == candidate.2)),
                "Ambiguous selected source scope"
            );
            if !sources.iter().any(|existing| existing.1 == candidate.1) {
                sources.push(candidate);
            }
        }
        Ok(sources)
    }

    /// Scope new download work to the authenticated account and complete
    /// provider database/zone identity. Rank proof is not an admission owner.
    pub(crate) fn download_scope(&self) -> Result<Option<(ShadowCapture, String, Value)>> {
        if !self.cross_zone_sources.is_empty() {
            return Ok(None);
        }
        self.authenticated_publication_scope()
    }

    fn authenticated_publication_scope(&self) -> Result<Option<(ShadowCapture, String, Value)>> {
        let Some((capture, database)) = &self.shadow_capture else {
            return Ok(None);
        };
        if !matches!(database.as_ref(), "private" | "shared")
            || self
                .zone_id
                .get("ownerRecordName")
                .and_then(Value::as_str)
                .is_none_or(|owner| owner.trim().is_empty())
        {
            return Ok(None);
        }
        Ok(Some((
            capture.clone(),
            capture.scope(database, &self.zone_id)?,
            self.zone_id.as_ref().clone(),
        )))
    }

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

#[cfg(test)]
#[path = "publication_scope_tests.rs"]
mod publication_scope_tests;
