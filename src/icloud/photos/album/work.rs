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

/// Exact provider resource requested from the already selected source.  The
/// authenticated selection owner still validates its generation, pass, policy
/// and native destination separately at dispatch.
pub(crate) struct RefreshTarget<'a> {
    pub(crate) database: &'a str,
    pub(crate) zone: &'a Value,
    pub(crate) child: &'a str,
    pub(crate) master: &'a str,
    pub(crate) version: crate::state::VersionSizeKey,
    pub(crate) checksum: &'a str,
    pub(crate) size: u64,
}

/// Upper bound for selected cleanup workers using global original-body permits.
/// The cleanup owner must enforce this bound while resolving requests.
pub(crate) const AUTH_REFRESH_MAX_IN_FLIGHT: usize = session::MAX_ORIGINAL_IN_FLIGHT_PAGES;

/// Per-task lookup plan. This is not source evidence or dispatch authority.
/// It does not hold a raw response or a global original-body permit. The owner
/// must move it to its exact selected worker; no clones or key overwrites.
pub(crate) struct RefreshRequest {
    album: PhotoAlbum,
    owner: crate::state::db::account::AccountOwner,
    database: String,
    zone: Value,
    child: String,
    master: String,
    version: crate::state::VersionSizeKey,
    checksum: String,
    size: u64,
}

impl RefreshRequest {
    pub(crate) fn new(
        album: PhotoAlbum,
        owner: crate::state::db::account::AccountOwner,
        target: &RefreshTarget<'_>,
    ) -> Self {
        Self {
            album,
            owner,
            database: target.database.to_owned(),
            zone: target.zone.clone(),
            child: target.child.to_owned(),
            master: target.master.to_owned(),
            version: target.version,
            checksum: target.checksum.to_owned(),
            size: target.size,
        }
    }

    /// Check this routing plan against the exact selected owner and task.
    /// This does not authenticate a URL or authorize dispatch.
    pub(crate) fn matches(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        target: &RefreshTarget<'_>,
    ) -> bool {
        let compatible = self.version == target.version
            || matches!(
                (self.version, target.version),
                (
                    crate::state::VersionSizeKey::Original,
                    crate::state::VersionSizeKey::Alternative
                ) | (
                    crate::state::VersionSizeKey::Alternative,
                    crate::state::VersionSizeKey::Original
                )
            );
        self.owner == *owner
            && self.database == target.database
            && self.zone == *target.zone
            && self.child == target.child
            && self.master == target.master
            && compatible
            && self.checksum == target.checksum
            && self.size == target.size
    }

    /// Consume the request and obtain a capability from the actual paired
    /// lookup. A missing capability holds only this selected task.
    pub(crate) async fn resolve(self) -> Result<Option<AuthenticatedRefresh>> {
        let target = RefreshTarget {
            database: &self.database,
            zone: &self.zone,
            child: &self.child,
            master: &self.master,
            version: self.version,
            checksum: &self.checksum,
            size: self.size,
        };
        let Some(witness) = self
            .album
            .authenticated_refresh(&self.owner, &target)
            .await?
        else {
            return Ok(None);
        };
        let Some(authority) = self.album.session.current_source_authority().await else {
            return Ok(None);
        };
        if !witness.matches(&self.owner, &authority, &target, witness.url()) {
            return Ok(None);
        }
        Ok(Some(witness))
    }
}

/// One actual authenticated paired lookup. It cannot be cloned or constructed
/// from a parsed Value, and owns the original transport permit until consumed.
/// This is an in-process capability, not a durable refresh/410 receipt.
#[must_use]
pub(crate) struct AuthenticatedRefresh {
    original: session::OriginalBody,
    invocation: session::AcceptedInvocation,
    request: String,
    source_binding: [u8; 32],
    database: String,
    zone: Value,
    child: String,
    master: String,
    version: crate::state::VersionSizeKey,
    checksum: String,
    size: u64,
    url: Box<str>,
    claimed: bool,
}

/// Borrowed non-authoritative source for bounded hint discovery only.
/// State must not accept this type as a claimed receipt or GET authority.
pub(crate) struct RefreshDiscoveryView<'a> {
    pub(crate) body: &'a [u8],
    pub(crate) request_body: &'a [u8],
    pub(crate) request_binding: [u8; 32],
    pub(crate) authority_binding: [u8; 32],
    pub(crate) invocation_id: uuid::Uuid,
}

/// Bounded exact provider source available only after this invocation has been
/// claimed by the selected owner. Its raw body may contain signed URLs and must
/// be charged, retained minimally, and never logged.
pub(crate) struct ClaimedRefreshSource<'a> {
    pub(crate) body: &'a [u8],
    /// The bounded lookup JSON only, never request URL or headers. State must
    /// validate both exact record names and the scoped zone on reopen.
    pub(crate) request_body: &'a [u8],
    pub(crate) request_binding: [u8; 32],
    pub(crate) authority_binding: [u8; 32],
    pub(crate) invocation_id: uuid::Uuid,
}

impl AuthenticatedRefresh {
    /// The actual provider rendition selected by this authenticated lookup.
    pub(crate) fn provider_version(&self) -> crate::state::VersionSizeKey {
        self.version
    }

    /// The caller must use this URL, rather than a URL derived from another
    /// lookup, for the selected task to which this capability is attached.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// Revalidate the live source fence and all provider facts at the dispatch
    /// boundary. Selection owner, path, policy and one-use routing are outside
    /// this capability and must be checked in the owning transaction.
    pub(crate) fn matches(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        authority: &session::SourceAuthority,
        target: &RefreshTarget<'_>,
        url: &str,
    ) -> bool {
        self.database == target.database
            && self.zone == *target.zone
            && self.child == target.child
            && self.master == target.master
            && self.version == target.version
            && self.checksum == target.checksum
            && self.size == target.size
            && self.url.as_ref() == url
            && self.invocation.matches_original(&self.original)
            && self.original.matches_authenticated_source(
                owner,
                target.database,
                "records/lookup",
                &self.request,
                &[("Content-type", "text/plain")],
            )
            && authority.binding(owner, target.database, "records/lookup")
                == Some(self.source_binding)
    }

    /// Borrow the same authenticated source for bounded predecessor
    /// discovery. This is a non-authoritative hint: it cannot authorize GET,
    /// state mutation or durable evidence, and does not consume the token.
    pub(crate) fn peek_source<'a>(
        &'a self,
        owner: &crate::state::db::account::AccountOwner,
        authority: &session::SourceAuthority,
        target: &RefreshTarget<'_>,
        url: &str,
    ) -> Option<RefreshDiscoveryView<'a>> {
        if self.claimed || !self.matches(owner, authority, target, url) {
            return None;
        }
        let request_binding = self.original.request_binding(
            owner,
            target.database,
            "records/lookup",
            &self.request,
            &[("Content-type", "text/plain")],
        )?;
        Some(RefreshDiscoveryView {
            body: self.original.as_bytes(),
            request_body: self.request.as_bytes(),
            request_binding,
            authority_binding: self.source_binding,
            invocation_id: self.invocation.id(),
        })
    }

    /// Source bytes and bindings may leave the capability only after its
    /// one-use claim and fresh authenticated-owner validation. State must bind
    /// the borrowed source to its exact generation/pass/path in one transaction.
    pub(crate) fn claimed_source<'a>(
        &'a self,
        owner: &crate::state::db::account::AccountOwner,
        authority: &session::SourceAuthority,
        target: &RefreshTarget<'_>,
        url: &str,
    ) -> Option<ClaimedRefreshSource<'a>> {
        if !self.claimed || !self.matches(owner, authority, target, url) {
            return None;
        }
        let request_binding = self.original.request_binding(
            owner,
            target.database,
            "records/lookup",
            &self.request,
            &[("Content-type", "text/plain")],
        )?;
        Some(ClaimedRefreshSource {
            body: self.original.as_bytes(),
            request_body: self.request.as_bytes(),
            request_binding,
            authority_binding: self.source_binding,
            invocation_id: self.invocation.id(),
        })
    }

    /// Claim the capability at the final owner transaction. A failed or
    /// uncertain dispatch cannot reuse this invocation for another attempt.
    pub(crate) fn claim(
        &mut self,
        owner: &crate::state::db::account::AccountOwner,
        authority: &session::SourceAuthority,
        target: &RefreshTarget<'_>,
        url: &str,
    ) -> bool {
        if self.claimed || !self.matches(owner, authority, target, url) {
            return false;
        }
        self.claimed = true;
        true
    }
}

fn has_current_pair_tags(body: &[u8], child: &str, master: &str) -> bool {
    let Ok(value) = changes_json::parse(body) else {
        return false;
    };
    let Some(records) = value.get("records").and_then(Value::as_array) else {
        return false;
    };
    records.len() == 2
        && [child, master].into_iter().all(|name| {
            let mut matches = records
                .iter()
                .filter(|r| r.get("recordName").and_then(Value::as_str) == Some(name));
            matches.next().is_some_and(|record| {
                record
                    .get("recordChangeTag")
                    .and_then(Value::as_str)
                    .is_some_and(|tag| !tag.trim().is_empty())
            }) && matches.next().is_none()
        })
}

impl PhotoAlbum {
    /// Snapshot the authenticated source again at final dispatch. This does
    /// not prove cookie-jar immutability or a complete redirect history.
    pub(crate) async fn refresh_source_authority(&self) -> Option<session::SourceAuthority> {
        self.session.current_source_authority().await
    }

    /// Mint a fresh-URL capability from this exact paired authenticated lookup.
    /// Missing transport evidence returns None and must leave selected work held.
    pub(crate) async fn authenticated_refresh(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        target: &RefreshTarget<'_>,
    ) -> Result<Option<AuthenticatedRefresh>> {
        if target.zone != self.zone_id.as_ref()
            || target.child == target.master
            || target.child.trim().is_empty()
            || target.master.trim().is_empty()
        {
            return Ok(None);
        }
        let url = format!(
            "{}/records/lookup?{}",
            self.service_endpoint,
            queries::encode_params(&self.params)
        );
        let request = json!({"records":[{"recordName":target.child},{"recordName":target.master}],
            "zoneID":self.zone_id.as_ref(),"desiredKeys":&*queries::IDENTITY_LOOKUP_KEYS_VALUES})
        .to_string();
        let response = session::retry_post_changes_with_evidence(
            self.session.as_ref(),
            &url,
            &request,
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;
        let (Some(original), Some(invocation)) = (response.original, response.accepted) else {
            return Ok(None);
        };
        if !invocation.matches_original(&original)
            || original.as_bytes() != response.body
            || !original.matches_authenticated_source(
                owner,
                target.database,
                "records/lookup",
                &request,
                &[("Content-type", "text/plain")],
            )
        {
            return Ok(None);
        }
        let Some(source_binding) = original.binding(
            owner,
            target.database,
            "records/lookup",
            &request,
            &[("Content-type", "text/plain")],
        ) else {
            return Ok(None);
        };
        let body = response.body;
        let child = target.child.to_owned();
        let master = target.master.to_owned();
        let zone = target.zone.clone();
        let expected_version = target.version;
        let checksum = target.checksum.to_owned();
        let expected_size = target.size;
        let selected_url = tokio::task::spawn_blocking(move || -> Result<Option<Box<str>>> {
            if !has_current_pair_tags(&body, &child, &master) {
                return Ok(None);
            }
            let asset = current_asset(&body, &child, &master, &zone)?;
            let mut candidates = asset.versions().iter().filter(|(version, resource)| {
                crate::state::VersionSizeKey::from(*version) == expected_version
                    && resource.checksum.as_ref() == checksum
                    && resource.size == expected_size
            });
            let Some((_, resource)) = candidates.next() else {
                return Ok(None);
            };
            if candidates.next().is_some() {
                return Ok(None);
            }
            Ok(Some(resource.url.clone()))
        })
        .await??;
        let Some(selected_url) = selected_url else {
            return Ok(None);
        };
        Ok(Some(AuthenticatedRefresh {
            original,
            invocation,
            request,
            source_binding,
            database: target.database.to_owned(),
            zone: target.zone.clone(),
            child: target.child.to_owned(),
            master: target.master.to_owned(),
            version: target.version,
            checksum: target.checksum.to_owned(),
            size: target.size,
            url: selected_url,
            claimed: false,
        }))
    }

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

#[cfg(test)]
mod authenticated_refresh_tests {
    use super::{PhotoAlbum, RefreshTarget};
    use crate::icloud::photos::album::PhotoAlbumConfig;
    use crate::icloud::photos::session::{
        ChangesResponse, InvocationToken, PhotosSession, SourceAuthority,
    };
    use crate::state::VersionSizeKey;
    use crate::state::db::account::AccountOwner;
    use serde_json::{Value, json};
    use std::sync::Arc;

    fn owner(id: &str) -> AccountOwner {
        AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":id}})).unwrap(),
        )
        .unwrap()
    }

    #[derive(Clone)]
    struct RawSession {
        owner: AccountOwner,
        bytes: Vec<u8>,
        replace_body: bool,
    }
    #[async_trait::async_trait]
    impl PhotosSession for RawSession {
        async fn post(&self, _: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
            panic!("authenticated test must use the original-body transport")
        }
        async fn post_changes_with_evidence(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
            token: InvocationToken,
        ) -> anyhow::Result<ChangesResponse> {
            let mut response = ChangesResponse::synthetic(
                &self.owner,
                "https://source.invalid",
                url,
                &body,
                headers,
                &self.bytes,
                token,
            )?;
            if self.replace_body {
                response.body = br#"{"records":[]}"#.to_vec();
            }
            Ok(response)
        }
        async fn current_source_authority(&self) -> Option<SourceAuthority> {
            SourceAuthority::synthetic(&self.owner, "https://source.invalid")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[derive(Clone)]
    struct ValueOnly;
    #[async_trait::async_trait]
    impl PhotosSession for ValueOnly {
        async fn post(&self, _: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
            Ok(json!({"records":[]}))
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn album(session: Box<dyn PhotosSession>, zone: &Value) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint:
                    "https://source.invalid/database/1/com.apple.photos.cloud/production/private"
                        .into(),
                name: "synthetic".into(),
                list_type: "synthetic".into(),
                obj_type: "synthetic".into(),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(zone.clone()),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            session,
        )
    }

    #[tokio::test]
    async fn authenticated_pair_yields_single_use_owner_bound_url() {
        let owner = owner("synthetic-refresh");
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let records = crate::test_helpers::mock_photo_records_for_zone_with_filename(
            "master",
            "PrimarySync",
            "test.jpg",
        );
        let raw = RawSession {
            owner: owner.clone(),
            bytes: serde_json::to_vec(&json!({"records":records})).unwrap(),
            replace_body: false,
        };
        let album = album(Box::new(raw), &zone);
        let target = RefreshTarget {
            database: "private",
            zone: &zone,
            child: "asset-master",
            master: "master",
            version: VersionSizeKey::Original,
            checksum: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            size: 1024,
        };
        let mut witness = album
            .authenticated_refresh(&owner, &target)
            .await
            .unwrap()
            .expect("authenticated original pair");
        let url = witness.url().to_owned();
        assert_eq!(url, "https://p01.icloud-content.com/master.jpg");
        let authority = SourceAuthority::synthetic(&owner, "https://source.invalid").unwrap();
        assert!(witness.matches(&owner, &authority, &target, &url));
        assert!(!witness.matches(
            &owner,
            &authority,
            &target,
            "https://example.invalid/different"
        ));
        let foreign = self::owner("foreign-refresh");
        assert!(!witness.matches(&foreign, &authority, &target, &url));
        assert!(
            witness
                .claimed_source(&owner, &authority, &target, &url)
                .is_none()
        );
        assert!(
            witness
                .peek_source(&owner, &authority, &target, &url)
                .is_some()
        );
        assert!(witness.claim(&owner, &authority, &target, &url));
        assert!(
            witness
                .peek_source(&owner, &authority, &target, &url)
                .is_none()
        );
        {
            let source = witness
                .claimed_source(&owner, &authority, &target, &url)
                .expect("claimed original source");
            let request = crate::icloud::photos::changes_json::parse(source.request_body).unwrap();
            assert_eq!(request["zoneID"], zone);
            assert_eq!(
                request["records"],
                json!([{"recordName":"asset-master"},{"recordName":"master"}])
            );
            let response = crate::icloud::photos::changes_json::parse(source.body).unwrap();
            assert_eq!(response["records"].as_array().unwrap().len(), 2);
            assert_ne!(source.request_binding, [0; 32]);
            assert_ne!(source.authority_binding, [0; 32]);
        }
        assert!(
            witness
                .claimed_source(&foreign, &authority, &target, &url)
                .is_none()
        );
        assert!(!witness.claim(&owner, &authority, &target, &url));
    }

    #[tokio::test]
    async fn request_resolves_scoped_witness_from_same_lookup() {
        let owner = owner("synthetic-refresh");
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let records = crate::test_helpers::mock_photo_records_for_zone_with_filename(
            "master",
            "PrimarySync",
            "test.jpg",
        );
        let raw = RawSession {
            owner: owner.clone(),
            bytes: serde_json::to_vec(&json!({"records":records})).unwrap(),
            replace_body: false,
        };
        let target = RefreshTarget {
            database: "private",
            zone: &zone,
            child: "asset-master",
            master: "master",
            version: VersionSizeKey::Original,
            checksum: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            size: 1024,
        };
        let request = super::RefreshRequest::new(album(Box::new(raw), &zone), owner, &target);
        let witness = request
            .resolve()
            .await
            .unwrap()
            .expect("same-call authenticated witness");
        assert_eq!(witness.url(), "https://p01.icloud-content.com/master.jpg");
    }

    #[tokio::test]
    async fn value_only_and_mismatched_response_cannot_mint_refresh() {
        let owner = owner("synthetic-refresh");
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let target = RefreshTarget {
            database: "private",
            zone: &zone,
            child: "asset-master",
            master: "master",
            version: VersionSizeKey::Original,
            checksum: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            size: 1024,
        };
        assert!(
            album(Box::new(ValueOnly), &zone)
                .authenticated_refresh(&owner, &target)
                .await
                .unwrap()
                .is_none()
        );
        let records = crate::test_helpers::mock_photo_records_for_zone_with_filename(
            "master",
            "PrimarySync",
            "test.jpg",
        );
        let raw = RawSession {
            owner: owner.clone(),
            bytes: serde_json::to_vec(&json!({"records":records})).unwrap(),
            replace_body: true,
        };
        assert!(
            album(Box::new(raw), &zone)
                .authenticated_refresh(&owner, &target)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pair_tags_reject_ambiguous_and_missing_tags() {
        let valid = br#"{"records":[{"recordName":"child","recordChangeTag":"c"},{"recordName":"master","recordChangeTag":"m"}]}"#;
        assert!(super::has_current_pair_tags(valid, "child", "master"));
        for invalid in [
            br#"{"records":[{"recordName":"child","recordChangeTag":"c"},{"recordName":"master","recordChangeTag":" "}]}"#.as_slice(),
            br#"{"records":[{"recordName":"child","recordChangeTag":"c"},{"recordName":"child","recordChangeTag":"m"}]}"#,
            br#"{"records":[{"recordName":"child","recordName":"other","recordChangeTag":"c"},{"recordName":"master","recordChangeTag":"m"}]}"#,
        ] { assert!(!super::has_current_pair_tags(invalid, "child", "master")); }
    }
}
