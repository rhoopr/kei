//! Current resource confirmation for retained catalog identities.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::Arc;

use super::PhotoAlbum;
use crate::icloud::photos::asset::{PhotoAsset, RequiredAssetFields};
use crate::icloud::photos::hydration_progress::{self, Phase};
use crate::icloud::photos::{changes_json, cloudkit, inbox::ShadowCapture, queries, session};

pub(crate) struct CurrentCatalogAsset {
    pub(crate) asset: PhotoAsset,
    /// Original paired lookup response, not cookies, headers or a URL identity.
    pub(crate) body: Vec<u8>,
}

fn scoped_record(record: &Value, zone: &Value) -> bool {
    let agrees = |actual: &Value| {
        actual.as_object().is_some_and(|fields| {
            fields
                .get("zoneName")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
                && fields
                    .iter()
                    .all(|(key, value)| zone.get(key) == Some(value))
        })
    };
    record.get("zoneID").is_none_or(agrees)
        && record
            .pointer("/fields/masterRef/value/zoneID")
            .is_none_or(agrees)
        && record
            .get("deleted")
            .is_none_or(|value| value.as_bool() == Some(false))
        && record.get("serverErrorCode").is_none_or(Value::is_null)
}

fn lookup_members(body: &[u8], names: &[&str], zone: &Value) -> Result<Vec<Value>> {
    let value = changes_json::parse(body)?;
    let records = value
        .get("records")
        .and_then(Value::as_array)
        .context("Missing current lookup records")?;
    anyhow::ensure!(
        records.len() == names.len(),
        "Incomplete current lookup members"
    );
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let mut matches = records
            .iter()
            .filter(|r| r.get("recordName").and_then(Value::as_str) == Some(name));
        let record = matches.next().context("Missing current lookup identity")?;
        anyhow::ensure!(
            matches.next().is_none() && scoped_record(record, zone),
            "Ambiguous or mismatched current lookup scope"
        );
        out.push(record.clone());
    }
    Ok(out)
}

/// Re-derive the paired current asset from retained bounded confirmation bytes.
/// Caller supplies the authenticated request scope; any supplied record or
/// relationship scope must agree. A complete pair is not an atomic snapshot.
pub(crate) fn current_asset(
    body: &[u8],
    child: &str,
    master: &str,
    zone: &Value,
) -> Result<PhotoAsset> {
    anyhow::ensure!(child != master, "Ambiguous current lookup pair");
    let records = lookup_members(body, &[child, master], zone)?;
    let [child_value, master_value] = records.as_slice() else {
        anyhow::bail!("Incomplete current lookup pair")
    };
    anyhow::ensure!(
        child_value.get("recordType").and_then(Value::as_str) == Some("CPLAsset")
            && master_value.get("recordType").and_then(Value::as_str) == Some("CPLMaster")
            && child_value
                .pointer("/fields/masterRef/value/recordName")
                .and_then(Value::as_str)
                == Some(master),
        "Current lookup pair mismatch"
    );
    let master: cloudkit::Record = serde_json::from_value(master_value.clone())?;
    let child: cloudkit::Record = serde_json::from_value(child_value.clone())?;
    let photo = PhotoAsset::try_from_records(master, &child, RequiredAssetFields::Downloadable)?;
    let mut photo = photo
        .with_state_record_name(Arc::from(child.record_name))
        .with_source_zone(Arc::from(
            zone.get("zoneName")
                .and_then(Value::as_str)
                .context("Missing current lookup zone")?,
        ));
    if let Some(witness) = changes_json::parse(body)?.get("selector_membership") {
        photo = photo.with_selector_membership(witness.clone());
    }
    Ok(photo)
}

pub(crate) fn same_selected_facts(left: &PhotoAsset, right: &PhotoAsset) -> bool {
    left.asset_record_name() == right.asset_record_name()
        && left.id() == right.id()
        && left.created() == right.created()
        && left.added_date() == right.added_date()
        && left.versions().len() == right.versions().len()
        && left.versions().iter().all(|(key, resource)| {
            right.versions().iter().any(|(other, value)| {
                other == key
                    && value.checksum == resource.checksum
                    && value.size == resource.size
                    && right
                        .metadata_arc(crate::state::VersionSizeKey::from(*other))
                        .metadata_hash
                        == left
                            .metadata_arc(crate::state::VersionSizeKey::from(*key))
                            .metadata_hash
            })
        })
}

impl PhotoAlbum {
    /// This first work stage admits only an unambiguous private library-wide
    /// source. Other passes retain their source facts for later qualification.
    pub(crate) fn catalog_work_scope(&self) -> Result<Option<(ShadowCapture, String, Value)>> {
        let Some((capture, database)) = &self.shadow_capture else {
            return Ok(None);
        };
        if database.as_ref() != "private"
            || self.container_id.is_some()
            || !self.cross_zone_sources.is_empty()
            || self.query_filter.is_some()
            || self.list_type.as_ref() != super::QUERY_ALL_LIST
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

    async fn current_lookup_body(&self, names: &[&str]) -> Result<Vec<u8>> {
        // Updates are inert outside an active anonymous hydration observation.
        hydration_progress::phase(if names.len() == 1 {
            Phase::AssetLookup
        } else {
            Phase::PairedLookup
        });
        let url = format!(
            "{}/records/lookup?{}",
            self.service_endpoint,
            queries::encode_params(&self.params)
        );
        let body = json!({"records":names.iter().map(|name|json!({"recordName":name})).collect::<Vec<_>>(),
            "zoneID":self.zone_id.as_ref(),"desiredKeys":&*queries::IDENTITY_LOOKUP_KEYS_VALUES});
        session::retry_post_changes_body(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await
    }

    /// Use only source zones already validated by album hydration. A name from
    /// another library does not authorize guessing a CloudKit zone.
    pub(crate) async fn confirm_layout_asset(
        &self,
        child: &str,
        zone: &str,
    ) -> Result<CurrentCatalogAsset> {
        if self.zone_name() == zone {
            return self.confirm_catalog_asset(child).await;
        }
        let mut sources = self
            .cross_zone_sources
            .iter()
            .filter(|source| source.zone_name() == zone);
        let source = sources
            .next()
            .context("Primary layout has no validated provider source zone")?;
        anyhow::ensure!(
            sources.next().is_none(),
            "Primary layout has ambiguous provider source zones"
        );
        source.confirm_catalog_asset(child).await
    }

    /// Resolve only a current authenticated owner link, never an unscoped
    /// record-name search across candidate zones.
    pub(crate) async fn confirm_selector_target(
        &self,
        selector_child: &str,
        expected: Option<&Value>,
    ) -> Result<PhotoAsset> {
        let scopes = self.selector_sources()?;
        anyhow::ensure!(!scopes.is_empty(), "Missing authenticated selecting owner");
        hydration_progress::source(1);
        let body = self.current_lookup_body(&[selector_child]).await?;
        let records = lookup_members(&body, &[selector_child], &self.zone_id)?;
        let record = records.first().context("Missing selecting child")?;
        anyhow::ensure!(
            record.get("recordType").and_then(Value::as_str) == Some("CPLAsset"),
            "Selecting child type mismatch"
        );
        let fields = record
            .get("fields")
            .context("Missing selecting child fields")?;
        let evidence = crate::icloud::photos::asset::SparseShareEvidence::from_fields(fields);
        let Some(crate::icloud::photos::asset::SparseShareEvidence::Linked(link)) = evidence else {
            anyhow::ensure!(
                evidence.is_none() && expected.is_none(),
                "Unresolved selecting source link"
            );
            return Ok(self.confirm_catalog_asset(selector_child).await?.asset);
        };
        let mut targets =
            self.cross_zone_sources
                .iter()
                .enumerate()
                .filter_map(|(index, source)| {
                    link.target_in_scope(&source.zone_id)
                        .map(|child| (index + 2, source, child))
                });
        let (source_index, source, target) = targets
            .next()
            .context("Selecting link has no authenticated attached source")?;
        anyhow::ensure!(
            targets.next().is_none(),
            "Selecting link has ambiguous attached source"
        );
        let (_, scope, zone) = scopes
            .iter()
            .find(|(_, _, zone)| zone == source.zone_id.as_ref())
            .context("Selected source scope is unavailable")?;
        let witness = json!({"version":2,"selector_zone":self.zone_id.as_ref(),"selector_child":selector_child,
            "child":target.as_str(),"source_zone":zone,"source_scope":scope});
        anyhow::ensure!(
            expected.is_none_or(|prior| [
                "selector_zone",
                "selector_child",
                "source_zone",
                "source_scope",
                "child"
            ]
            .into_iter()
            .all(|key| prior.get(key) == witness.get(key))),
            "Selecting source binding changed"
        );
        hydration_progress::source(source_index);
        let asset = source.confirm_catalog_asset(target.as_str()).await?.asset;
        Ok(asset.with_selector_membership(witness))
    }

    /// Carry the exact selecting relation through the delta routing boundary.
    /// A child ID alone cannot supply membership for a different source.
    pub(crate) async fn confirm_delta_membership(
        &self,
        asset: PhotoAsset,
        events: &[crate::icloud::photos::asset::ChangeEvent],
    ) -> Result<PhotoAsset> {
        let container = self
            .container_id
            .as_deref()
            .context("Missing selected album")?;
        let source_witness = asset
            .selector_membership()
            .context("Missing authenticated source link")?;
        let selector_child = source_witness
            .get("selector_child")
            .and_then(Value::as_str)
            .context("Missing selecting child identity")?;
        let mut relations = std::collections::BTreeMap::new();
        for event in events {
            // Last observation wins, including tombstones lacking fields.
            relations.insert(event.record_name.as_ref(), event);
        }
        let event = relations
            .values()
            .find(|event| {
                event.relation.as_ref().is_some_and(|relation| {
                    !relation.is_deleted
                        && relation.container_id.as_ref() == container
                        && relation.asset_record_name.as_ref() == selector_child
                })
            })
            .context("Missing current selected relation observation")?;
        let mut witness = source_witness.clone();
        let fields = witness
            .as_object_mut()
            .context("Invalid source link witness")?;
        fields.insert("kind".into(), json!("relation_observation"));
        fields.insert("container".into(), json!(container));
        fields.insert("relation".into(), json!(event.record_name));
        self.confirm_selector_membership(&witness, asset.asset_record_name())
            .await?;
        Ok(asset.with_selector_membership(witness))
    }

    /// Refresh membership for an existing fixed-source obligation. This lookup
    /// does not discover or choose a resource source.
    pub(crate) async fn confirm_selector_membership(
        &self,
        witness: &Value,
        child: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            witness.get("version").and_then(Value::as_u64) == Some(2)
                && witness.get("selector_zone") == Some(self.zone_id.as_ref())
                && witness.get("container").and_then(Value::as_str) == self.container_id.as_deref()
                && witness.get("child").and_then(Value::as_str) == Some(child),
            "Selecting membership scope mismatch"
        );
        let selector_child = witness
            .get("selector_child")
            .and_then(Value::as_str)
            .filter(|child| !child.is_empty())
            .context("Missing selecting child")?;
        let relation = witness
            .get("relation")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .context("Missing retained relation identity")?;
        let url = format!(
            "{}/records/lookup?{}",
            self.service_endpoint,
            queries::encode_params(&self.params)
        );
        let request = json!({"zoneID":self.zone_id.as_ref(),"records":[{"recordName":relation}],"desiredKeys":["containerId","itemId"]});
        let body = session::retry_post_changes_body(
            self.session.as_ref(),
            &url,
            &request.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;
        let records = lookup_members(&body, &[relation], &self.zone_id)?;
        let record = records.first().context("Missing current album relation")?;
        anyhow::ensure!(
            record.get("recordType").and_then(Value::as_str) == Some("CPLContainerRelation")
                && record
                    .pointer("/fields/containerId/value")
                    .and_then(Value::as_str)
                    == self.container_id.as_deref()
                && record
                    .pointer("/fields/itemId/value")
                    .and_then(Value::as_str)
                    == Some(selector_child),
            "Current selecting membership differs"
        );
        let current = self
            .confirm_selector_target(selector_child, Some(witness))
            .await?;
        let binding = current
            .selector_membership()
            .context("Selecting child no longer names the retained source")?;
        anyhow::ensure!(
            [
                "selector_zone",
                "selector_child",
                "source_zone",
                "source_scope",
                "child"
            ]
            .into_iter()
            .all(|key| binding.get(key) == witness.get(key)),
            "Selecting source binding changed"
        );
        Ok(())
    }

    pub(crate) async fn confirm_catalog_asset(&self, child: &str) -> Result<CurrentCatalogAsset> {
        let body = self.current_lookup_body(&[child]).await?;
        let zone = self.zone_id.as_ref().clone();
        let requested = child.to_owned();
        let master = tokio::task::spawn_blocking(move || {
            let records = lookup_members(&body, &[&requested], &zone)?;
            let child_value = records.first().context("Missing current lookup child")?;
            anyhow::ensure!(
                child_value.get("recordType").and_then(Value::as_str) == Some("CPLAsset"),
                "Current lookup child type mismatch"
            );
            Ok::<_, anyhow::Error>(
                child_value
                    .pointer("/fields/masterRef/value/recordName")
                    .and_then(Value::as_str)
                    .filter(|name| !name.trim().is_empty())
                    .context("Unresolved current master identity")?
                    .to_owned(),
            )
        })
        .await??;
        let body = self.current_lookup_body(&[child, &master]).await?;
        let zone = self.zone_id.as_ref().clone();
        let child = child.to_owned();
        tokio::task::spawn_blocking(move || {
            let asset = current_asset(&body, &child, &master, &zone)?;
            Ok(CurrentCatalogAsset { asset, body })
        })
        .await?
    }
}
