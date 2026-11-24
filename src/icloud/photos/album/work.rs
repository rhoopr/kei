//! Current resource confirmation for retained catalog identities.

use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use std::sync::Arc;

use super::PhotoAlbum;
use crate::icloud::photos::asset::{PhotoAsset, RequiredAssetFields};
#[cfg(test)]
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{changes_json, cloudkit, queries, session};
use crate::state::db::provider_selection::SelectionMediaSource;

#[cfg(test)]
mod tests;

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
    let mut value = changes_json::parse(body)?;
    let records = value
        .get_mut("records")
        .and_then(Value::as_array_mut)
        .context("Missing current lookup records")?;
    anyhow::ensure!(
        records.len() == names.len(),
        "Incomplete current lookup members"
    );
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let mut matches = records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.get("recordName").and_then(Value::as_str) == Some(name));
        let (index, record) = matches.next().context("Missing current lookup identity")?;
        anyhow::ensure!(
            matches.next().is_none() && scoped_record(record, zone),
            "Ambiguous or mismatched current lookup scope"
        );
        out.push(records.swap_remove(index));
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
    let [child_value, master_value]: [Value; 2] = records
        .try_into()
        .map_err(|_unexpected_records| anyhow::anyhow!("Incomplete current lookup pair"))?;
    anyhow::ensure!(
        child_value.get("recordType").and_then(Value::as_str) == Some("CPLAsset")
            && master_value.get("recordType").and_then(Value::as_str) == Some("CPLMaster")
            && child_value
                .pointer("/fields/masterRef/value/recordName")
                .and_then(Value::as_str)
                == Some(master),
        "Current lookup pair mismatch"
    );
    let master: cloudkit::Record = serde_json::from_value(master_value)?;
    let child: cloudkit::Record = serde_json::from_value(child_value)?;
    let photo = PhotoAsset::try_from_records(master, &child, RequiredAssetFields::Downloadable)?;
    Ok(photo
        .with_state_record_name(Arc::from(child.record_name))
        .with_source_zone(Arc::from(
            zone.get("zoneName")
                .and_then(Value::as_str)
                .context("Missing current lookup zone")?,
        )))
}

#[derive(serde::Deserialize)]
struct RawLookupResponse<'a> {
    #[serde(borrow)]
    records: Vec<&'a RawValue>,
}

#[derive(serde::Deserialize)]
struct RawRecordName {
    #[serde(rename = "recordName")]
    name: String,
}

/// Index the exact provider record slices from an already validated lookup
/// response. `lookup_members` runs `changes_json` first, so duplicate keys and
/// ambiguous identities have already been refused before this borrowed parse.
fn raw_lookup_records(body: &[u8]) -> Result<FxHashMap<String, &RawValue>> {
    let response: RawLookupResponse<'_> = serde_json::from_slice(body)?;
    let mut by_name = FxHashMap::default();
    for record in response.records {
        let RawRecordName { name } = serde_json::from_str(record.get())?;
        anyhow::ensure!(
            by_name.insert(name, record).is_none(),
            "Ambiguous or mismatched current lookup scope"
        );
    }
    Ok(by_name)
}

fn pair_confirmation_body(child: &RawValue, master: &RawValue) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(child.get().len() + master.get().len() + 16);
    body.extend_from_slice(b"{\"records\":[");
    body.extend_from_slice(child.get().as_bytes());
    body.push(b',');
    body.extend_from_slice(master.get().as_bytes());
    body.extend_from_slice(b"]}");
    anyhow::ensure!(
        body.len() <= crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
        "Current confirmation exceeded the provider evidence byte limit"
    );
    Ok(body)
}

fn add_pair_proof_bytes(total: &mut usize, proof_len: usize) -> Result<()> {
    let next = total
        .checked_add(proof_len)
        .context("Current confirmation batch exceeded the provider evidence byte limit")?;
    anyhow::ensure!(
        next <= crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
        "Current confirmation batch exceeded the provider evidence byte limit"
    );
    *total = next;
    Ok(())
}

fn confirm_batch_response(
    body: &[u8],
    zone: &Value,
    pairs: &[(String, String)],
    names: &[String],
) -> Result<Vec<CurrentCatalogAsset>> {
    let requested: Vec<&str> = names.iter().map(String::as_str).collect();
    // Strict `changes_json` duplicate-key validation and exact requested
    // names, no extras, no omissions and no duplicates.
    lookup_members(body, &requested, zone)?;
    let raw = raw_lookup_records(body)?;
    anyhow::ensure!(
        raw.len() == names.len(),
        "Incomplete current lookup members"
    );
    let mut out = Vec::with_capacity(pairs.len());
    let mut total_proof_bytes = 0usize;
    for (child_name, master_name) in pairs {
        let child = raw
            .get(child_name)
            .copied()
            .context("Missing current lookup child")?;
        let master = raw
            .get(master_name)
            .copied()
            .context("Missing current lookup master")?;
        // Each returned body is independently valid for the existing current
        // asset contract and keeps the provider's exact record bytes. Shared
        // masters expand per child, so the whole derived batch stays within one
        // provider page budget before any evidence is returned.
        let confirmation = pair_confirmation_body(child, master)?;
        add_pair_proof_bytes(&mut total_proof_bytes, confirmation.len())?;
        let asset = current_asset(&confirmation, child_name, master_name, zone)?;
        out.push(CurrentCatalogAsset {
            asset,
            body: confirmation,
        });
    }
    Ok(out)
}

pub(crate) fn same_selected_facts(left: &PhotoAsset, right: &PhotoAsset) -> bool {
    // Unscoped rank observations inherit their validated query scope at the
    // caller. Two explicit media sources cannot prove each other's identity.
    left.source_zone()
        .zip(right.source_zone())
        .is_none_or(|(left, right)| left == right)
        && left.asset_record_name() == right.asset_record_name()
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
    /// Confirm a selected observation in its bound source, rather than treating
    /// equal child names in the membership and media libraries as one identity.
    pub(crate) async fn confirm_selected_asset(
        &self,
        observed: &PhotoAsset,
    ) -> Result<CurrentCatalogAsset> {
        let source = self.selected_media_album(observed)?;
        self.bound_media_source(source)?;
        // The observation supplies both lookup keys, not proof of their
        // relationship. Verify that relationship in the fresh paired reply.
        source
            .confirm_pair(observed.asset_record_name(), observed.id())
            .await
    }

    pub(crate) fn selected_media_source(
        &self,
        observed: &PhotoAsset,
    ) -> Result<SelectionMediaSource> {
        self.bound_media_source(self.selected_media_album(observed)?)
    }

    /// Freeze only sources sharing this selection's authenticated account/store.
    pub(crate) fn selection_media_sources(&self) -> Result<Vec<SelectionMediaSource>> {
        let mut zones = rustc_hash::FxHashSet::default();
        zones.insert(self.zone_name());
        let mut sources = Vec::with_capacity(self.cross_zone_sources.len());
        for album in &self.cross_zone_sources {
            anyhow::ensure!(
                zones.insert(album.zone_name()),
                "Selected source is ambiguous"
            );
            sources.push(self.bound_media_source(album)?);
        }
        sources.sort_by(|left, right| left.scope.cmp(&right.scope));
        Ok(sources)
    }

    pub(crate) async fn confirm_selection_identity(
        &self,
        child: &str,
        binding: &SelectionMediaSource,
    ) -> Result<CurrentCatalogAsset> {
        let zone = binding
            .zone
            .get("zoneName")
            .and_then(Value::as_str)
            .context("Selected media source has no zone")?;
        let source = self.media_album(zone)?;
        anyhow::ensure!(
            self.bound_media_source(source)? == *binding,
            "Selected media source binding changed"
        );
        source.confirm_catalog_asset(child).await
    }

    fn selected_media_album(&self, observed: &PhotoAsset) -> Result<&Self> {
        let zone = observed
            .source_zone()
            // Owner-zone rank observations inherit their authenticated query
            // scope; hydration attaches an explicit source to foreign assets.
            .or_else(|| self.zone_id.get("zoneName").and_then(Value::as_str))
            .context("Selected observation has no provider source")?;
        self.media_album(zone)
    }

    fn media_album(&self, zone: &str) -> Result<&Self> {
        let source = if self.zone_id.get("zoneName").and_then(Value::as_str) == Some(zone) {
            self
        } else {
            let mut sources = self.cross_zone_sources.iter().filter(|source| {
                source.zone_id.get("zoneName").and_then(Value::as_str) == Some(zone)
            });
            let source = sources
                .next()
                .context("Selected source is not bound to this album")?;
            anyhow::ensure!(sources.next().is_none(), "Selected source is ambiguous");
            source
        };
        Ok(source)
    }

    fn bound_media_source(&self, source: &Self) -> Result<SelectionMediaSource> {
        let (owner, _, _) = self
            .owned_selection_scope()?
            .context("Selection owner is not authenticated")?;
        let (media_owner, scope, zone) = source
            .owned_selection_scope()?
            .context("Selected media source is not authenticated")?;
        anyhow::ensure!(
            owner.owner == media_owner.owner
                && Arc::ptr_eq(&owner.db, &media_owner.db)
                && owner.scope("private", &zone)? == media_owner.scope("private", &zone)?,
            "Selected media source ownership mismatch"
        );
        anyhow::ensure!(
            !source.zone_name().trim().is_empty(),
            "Selected media source has no zone"
        );
        Ok(SelectionMediaSource { scope, zone })
    }

    /// This first work stage admits only an unambiguous private library-wide
    /// source. Other passes retain their source facts for later qualification.
    #[cfg(test)]
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
        self.confirm_pair(child, &master).await
    }

    /// Confirm one bounded group of unfiled observations in their known own
    /// source. Every member must share this album's bound zone; shared masters
    /// are requested once. A malformed, incomplete, ambiguous, or out-of-scope
    /// reply refuses the whole group so callers can fall back to individual
    /// current lookups without consuming partial evidence.
    pub(crate) async fn confirm_unfiled_batch(
        &self,
        observed: &[&PhotoAsset],
    ) -> Result<Vec<CurrentCatalogAsset>> {
        if observed.is_empty() {
            return Ok(Vec::new());
        }
        anyhow::ensure!(
            observed.len() <= Self::CONFIRMATION_BATCH_SIZE,
            "Current confirmation batch exceeds the provider lookup bound"
        );
        let zone_name = self.zone_name();
        anyhow::ensure!(
            !zone_name.trim().is_empty(),
            "Current confirmation has no bound provider source"
        );
        // The batch trusts only this album's authenticated source. Validate the
        // account/database/scope binding once before any provider request.
        self.bound_media_source(self)?;
        let mut names = Vec::with_capacity(observed.len().saturating_mul(2));
        let mut seen = FxHashSet::default();
        let mut pairs = Vec::with_capacity(observed.len());
        for asset in observed {
            let child = asset.asset_record_name();
            let master = asset.id();
            anyhow::ensure!(
                !child.trim().is_empty() && !master.trim().is_empty() && child != master,
                "Invalid current lookup pair"
            );
            anyhow::ensure!(
                asset.source_zone().is_none_or(|source| source == zone_name),
                "Current confirmation observation is bound to another source"
            );
            for name in [child, master] {
                if seen.insert(name) {
                    names.push(name.to_owned());
                }
            }
            pairs.push((child.to_owned(), master.to_owned()));
        }
        if let [(child, master)] = pairs.as_slice() {
            return self
                .confirm_pair(child, master)
                .await
                .map(|asset| vec![asset]);
        }
        let requested: Vec<&str> = names.iter().map(String::as_str).collect();
        let body = self.current_lookup_body(&requested).await?;
        anyhow::ensure!(
            body.len() <= crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
            "Current confirmation batch exceeded the provider evidence byte limit"
        );
        let zone = self.zone_id.as_ref().clone();
        tokio::task::spawn_blocking(move || confirm_batch_response(&body, &zone, &pairs, &names))
            .await?
    }

    async fn confirm_pair(&self, child: &str, master: &str) -> Result<CurrentCatalogAsset> {
        anyhow::ensure!(
            !child.trim().is_empty() && !master.trim().is_empty() && child != master,
            "Invalid current lookup pair"
        );
        let body = self.current_lookup_body(&[child, master]).await?;
        let zone = self.zone_id.as_ref().clone();
        let child = child.to_owned();
        let master = master.to_owned();
        tokio::task::spawn_blocking(move || {
            let asset = current_asset(&body, &child, &master, &zone)?;
            Ok(CurrentCatalogAsset { asset, body })
        })
        .await?
    }
}
