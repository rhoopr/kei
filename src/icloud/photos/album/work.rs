//! Current resource confirmation for retained catalog identities.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::Arc;

use super::PhotoAlbum;
use crate::icloud::photos::asset::{PhotoAsset, RequiredAssetFields};
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
    Ok(photo
        .with_state_record_name(Arc::from(child.record_name))
        .with_source_zone(Arc::from(
            zone.get("zoneName")
                .and_then(Value::as_str)
                .context("Missing current lookup zone")?,
        )))
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
