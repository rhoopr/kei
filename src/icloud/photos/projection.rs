//! Versioned source facts, not selection, queue completion or coverage policy.

use anyhow::Context;
use serde_json::Value;

use super::inbox::{MAX_CHANGES_PAGE_BYTES, ShadowCapture};
use crate::state::db::provider_catalog::{
    CatalogDebt, CatalogPlan, CatalogRecord, CatalogReference,
};
use crate::state::db::provider_inbox::StoredPage;
use crate::state::error::StateError;

fn charge(total: &mut u64, bytes: usize) -> anyhow::Result<()> {
    *total = total
        .checked_add(bytes as u64)
        .ok_or(StateError::ProviderCatalogFull)?;
    anyhow::ensure!(
        *total <= MAX_CHANGES_PAGE_BYTES as u64,
        StateError::ProviderCatalogFull
    );
    Ok(())
}

fn debt(
    record: &mut CatalogRecord,
    total: &mut u64,
    path: &str,
    reason: &'static str,
) -> anyhow::Result<()> {
    charge(total, path.len() + reason.len() + 32)?;
    record.debt.push(CatalogDebt {
        path: path.to_owned(),
        reason,
    });
    Ok(())
}

fn reference(
    record: &mut CatalogRecord,
    total: &mut u64,
    path: &str,
    target: &str,
    zone: Option<&Value>,
) -> anyhow::Result<()> {
    let zone_name = zone
        .and_then(|z| z.get("zoneName"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let zone_owner = zone
        .and_then(|z| z.get("ownerRecordName"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if zone.is_some_and(|z| {
        !z.is_object()
            || z.get("zoneName")
                .and_then(Value::as_str)
                .is_none_or(|name| name.trim().is_empty())
            || z.get("ownerRecordName")
                .is_some_and(|owner| owner.as_str().is_none_or(|name| name.trim().is_empty()))
    }) {
        debt(record, total, path, "malformed_reference_scope")?;
    }
    charge(
        total,
        path.len()
            + target.len()
            + zone_name.as_ref().map_or(0, String::len)
            + zone_owner.as_ref().map_or(0, String::len)
            + 64,
    )?;
    record.references.push(CatalogReference {
        path: path.to_owned(),
        target: target.to_owned(),
        zone_name,
        zone_owner,
    });
    // A literal reference is not proof of its target's current identity or
    // materialization. Future resolution must retain this source provenance.
    debt(record, total, path, "unresolved_reference")
}

fn component(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn walk(
    value: &Value,
    path: &mut String,
    record: &mut CatalogRecord,
    total: &mut u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.len() <= MAX_CHANGES_PAGE_BYTES,
        StateError::ProviderCatalogFull
    );
    match value {
        Value::Object(fields) => {
            if let Some(name) = fields.get("recordName") {
                if let Some(name) = name.as_str().filter(|name| !name.trim().is_empty()) {
                    reference(record, total, path, name, fields.get("zoneID"))?;
                } else {
                    debt(record, total, path, "malformed_reference")?;
                }
            }
            for (key, value) in fields {
                walk_child(value, &component(key), path, record, total)?;
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                walk_child(value, &index.to_string(), path, record, total)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// One bounded scratch path prevents nested unknown values from retaining a
// fresh copy of every ancestor path. Stored paths are charged before copying.
fn walk_child(
    value: &Value,
    suffix: &str,
    path: &mut String,
    record: &mut CatalogRecord,
    total: &mut u64,
) -> anyhow::Result<()> {
    let end = path.len();
    anyhow::ensure!(
        end.checked_add(suffix.len())
            .and_then(|n| n.checked_add(1))
            .is_some_and(|n| n <= MAX_CHANGES_PAGE_BYTES),
        StateError::ProviderCatalogFull
    );
    path.push('/');
    path.push_str(suffix);
    walk(value, path, record, total)?;
    path.truncate(end);
    Ok(())
}

pub(super) fn plan(capture: &ShadowCapture, source: StoredPage) -> anyhow::Result<CatalogPlan> {
    let scope: Value = serde_json::from_str(&source.page.scope)
        .map_err(|_invalid_scope| StateError::ProviderCatalogInvalid)?;
    let database = scope
        .get("database")
        .and_then(Value::as_str)
        .context("Missing catalog database scope")?;
    let body = super::changes_json::parse(&source.page.body)?;
    let zone = body
        .get("zones")
        .and_then(Value::as_array)
        .and_then(|zones| zones.first())
        .and_then(|zone| zone.get("zoneID"))
        .context("Missing catalog returned scope")?;
    anyhow::ensure!(
        capture.scope(database, zone)? == source.page.scope,
        StateError::ProviderCatalogInvalid
    );
    let validated = super::album::catalog_observed_page(
        source.page.body.clone(),
        &source.page.scope,
        &source.page.request_cursor,
    )?;
    anyhow::ensure!(validated == source.page, StateError::ProviderCatalogInvalid);
    let records = body
        .get("zones")
        .and_then(Value::as_array)
        .and_then(|z| z.first())
        .and_then(|z| z.get("records"))
        .and_then(Value::as_array)
        .context("Missing catalog source records")?;
    let mut charged_bytes = 128;
    let mut projected = Vec::with_capacity(records.len());
    for (value, identity) in records.iter().zip(&source.page.identities) {
        let kind = match identity.record_type.as_deref() {
            Some("CPLMaster") => "master",
            Some("CPLAsset") => "asset",
            Some("CPLAlbum") => "album",
            Some("CPLContainerRelation") => "relation",
            _ => "unknown",
        };
        charge(
            &mut charged_bytes,
            identity.name.len()
                + identity.record_type.as_ref().map_or(0, String::len)
                + kind.len()
                + 64,
        )?;
        let mut record = CatalogRecord {
            kind,
            references: Vec::new(),
            debt: Vec::new(),
        };
        if kind == "unknown" {
            debt(&mut record, &mut charged_bytes, "", "unclassified_record")?;
        }
        if let Some(fields) = value.get("fields").and_then(Value::as_object) {
            for (name, field) in fields {
                let mut path = format!("/fields/{}/value", component(name));
                if let Some(value) = field.get("value") {
                    if kind == "relation"
                        && matches!(name.as_str(), "containerId" | "itemId")
                        && let Some(target) =
                            value.as_str().filter(|target| !target.trim().is_empty())
                    {
                        reference(&mut record, &mut charged_bytes, &path, target, None)?;
                    }
                    if field.get("type").and_then(Value::as_str) == Some("REFERENCE")
                        && value.get("recordName").is_none()
                    {
                        debt(
                            &mut record,
                            &mut charged_bytes,
                            &path,
                            "malformed_reference",
                        )?;
                    }
                    walk(value, &mut path, &mut record, &mut charged_bytes)?;
                } else if field.get("type").and_then(Value::as_str) == Some("REFERENCE") {
                    debt(
                        &mut record,
                        &mut charged_bytes,
                        &path,
                        "malformed_reference",
                    )?;
                }
            }
        }
        if !identity.deleted {
            if kind == "asset"
                && value
                    .pointer("/fields/masterRef/value/recordName")
                    .and_then(Value::as_str)
                    .is_none_or(|name| name.trim().is_empty())
            {
                debt(
                    &mut record,
                    &mut charged_bytes,
                    "/fields/masterRef",
                    "incomplete_asset_link",
                )?;
            }
            if kind == "relation"
                && ["containerId", "itemId"].iter().any(|name| {
                    value
                        .get("fields")
                        .and_then(|fields| fields.get(*name))
                        .and_then(|field| field.get("value"))
                        .and_then(Value::as_str)
                        .is_none_or(|name| name.trim().is_empty())
                })
            {
                debt(
                    &mut record,
                    &mut charged_bytes,
                    "/fields",
                    "incomplete_relation",
                )?;
            }
        }
        projected.push(record);
    }
    Ok(CatalogPlan {
        source,
        records: projected,
        charged_bytes,
    })
}
