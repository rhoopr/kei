//! Bounded, causal pre-generation zone observations. This module is a draft:
//! it is not wired into ingestion or state admission, and does not by itself
//! authorize a shared wait.
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::cloudkit::Record;
use super::session::OriginalBody;
use crate::state::db::account::AccountOwner;

/// Opaque source authority created from an actual selected PhotoAlbum and its
/// pinned session. State must still revalidate this snapshot at consumption.
#[derive(Clone)]
pub(crate) struct AuthenticatedZoneScope {
    owner: AccountOwner,
    scope: String,
    zone: Value,
    database: String,
    authority: super::session::SourceAuthority,
}
impl AuthenticatedZoneScope {
    pub(super) fn from_album(
        owner: AccountOwner,
        scope: String,
        zone: Value,
        database: String,
        authority: super::session::SourceAuthority,
    ) -> Option<Self> {
        let parsed: Value = serde_json::from_str(&scope).ok()?;
        let scoped_zone = parsed.get("zone")?.as_object()?;
        let zone_object = zone.as_object()?;
        if ["zoneName", "ownerRecordName"].iter().any(|key| {
            scoped_zone
                .get(*key)
                .and_then(Value::as_str)
                .is_none_or(|v| v.trim().is_empty())
                || scoped_zone.get(*key) != zone_object.get(*key)
        }) || scoped_zone
            .iter()
            .any(|(key, value)| zone_object.get(key) != Some(value))
            || parsed.get("database")?.as_str()? != database
            || authority
                .binding(&owner, &database, "changes/zone")
                .is_none()
        {
            return None;
        }
        Some(Self {
            owner,
            scope,
            zone,
            database,
            authority,
        })
    }
    pub(crate) fn owner(&self) -> &AccountOwner {
        &self.owner
    }
    pub(crate) fn scope(&self) -> &str {
        &self.scope
    }
    pub(crate) fn zone(&self) -> &Value {
        &self.zone
    }
    pub(crate) fn database(&self) -> &str {
        &self.database
    }
    pub(crate) fn authority(&self) -> &super::session::SourceAuthority {
        &self.authority
    }
}
// A response may omit descriptive request fields. Every field it does
// supply must agree, and the complete scoped identity must be returned.
fn response_zone_matches(response: &Value, requested: &Value) -> bool {
    let (Some(response), Some(requested)) = (response.as_object(), requested.as_object()) else {
        return false;
    };
    ["zoneName", "ownerRecordName"].iter().all(|key| {
        requested
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| {
                !value.trim().is_empty()
                    && response.get(*key).and_then(Value::as_str) == Some(value)
            })
    }) && response
        .iter()
        .all(|(key, value)| requested.get(key) == Some(value))
}

#[derive(Debug, thiserror::Error)]
#[error("invalid authenticated source ordinal")]
pub(crate) struct InvalidSourceOrdinal;

// ShadowPageError is transparent: its source() forwards past the inner
// anyhow root. This explicit source preserves the typed ordinal cause.
#[derive(Debug, thiserror::Error)]
#[error("invalid authenticated source ordinal")]
struct OrdinalRefusal(#[source] InvalidSourceOrdinal);

struct Stage {
    #[cfg(test)]
    invalid_ordinal: std::sync::atomic::AtomicBool,
    epoch: uuid::Uuid,
    context: AuthenticatedZoneScope,
    passes: HashSet<String>,
    // Bounded conservative one-shot registry; overflow disables proof only.
    pairs: Mutex<HashSet<(uuid::Uuid, usize, uuid::Uuid, usize)>>,
}
/// One actual incremental zone enumeration. Cloning shares identity.
#[derive(Clone)]
pub(crate) struct ZoneStage(Arc<Stage>);

/// One request issued by that enumeration. A successful page consumes it.
pub(crate) struct PageRequest {
    stage: ZoneStage,
    body: String,
    headers: Vec<(String, String)>,
}

struct Page {
    id: uuid::Uuid,
    stage: ZoneStage,
    original: OriginalBody,
    // Shared by all records on this page. The non-Clone capability can only
    // originate from the exact transport invocation accepted by the wrapper.
    invocation: super::session::AcceptedInvocation,
    records: Vec<Value>,
}

/// Issued only for a typed record decoded at the actual page ordinal.
#[derive(Clone)]
pub(crate) struct SourceOrdinal {
    page: Arc<Page>,
    ordinal: usize,
}

impl SourceOrdinal {
    pub(super) fn raw_record(&self) -> Option<&Value> {
        self.page.records.get(self.ordinal)
    }
}

impl std::fmt::Debug for SourceOrdinal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted source ordinal>")
    }
}

/// The source records actually consumed by the pair constructor. It is shared
/// with the corresponding asset, not reconstructed from later confirmation.
pub(crate) struct PairedObservation {
    master_page: Arc<Page>,
    master_ordinal: usize,
    child_page: Arc<Page>,
    child_ordinal: usize,
    routed: Mutex<HashSet<String>>,
}

impl std::fmt::Debug for PairedObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted paired observation>")
    }
}

/// A route candidate; neither variant is itself admission authority. State
/// must prove that a direct-zone pass really selects this zone without an
/// unproven album/exclusion condition, or validate the selector witness.
#[derive(Clone)]
pub(crate) enum RouteWitness {
    DirectZone,
    Selector(Value),
}

/// One use of a pair by a real pass. Membership is only input to the state
/// verifier; the presence of a witness here does not establish membership.
pub(crate) struct RoutedObservation {
    pair: Arc<PairedObservation>,
    pass: String,
    witness: RouteWitness,
}

/// Owned, non-Clone view minted only from a real routed observation. The
/// originating route lease must remain held through the state transaction.
pub(crate) struct CompactCandidate {
    pair: Arc<PairedObservation>,
    pass: String,
    witness: RouteWitness,
    lease_active: Arc<std::sync::atomic::AtomicBool>,
}

/// Immutable view derived from an actual staged typed record and its exact
/// transport invocation. There is no constructor from independent values.
pub(crate) struct PageAttestation<'a> {
    invocation: uuid::Uuid,
    ordinal: usize,
    record: &'a Value,
    request: &'a super::session::RetainedRequest,
    request_binding: [u8; 32],
    authority_binding: [u8; 32],
}
impl<'a> PageAttestation<'a> {
    pub(crate) fn fields(
        &self,
    ) -> (
        uuid::Uuid,
        usize,
        &'a Value,
        &'a super::session::RetainedRequest,
        [u8; 32],
        [u8; 32],
    ) {
        (
            self.invocation,
            self.ordinal,
            self.record,
            self.request,
            self.request_binding,
            self.authority_binding,
        )
    }
}

impl CompactCandidate {
    pub(crate) fn is_leased(&self) -> bool {
        self.lease_active.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Exact canonical scope of the staged source, never caller-supplied.
    pub(crate) fn source_scope(&self) -> &str {
        &self.pair.master_page.stage.0.context.scope
    }

    pub(crate) fn stage_epoch(&self) -> uuid::Uuid {
        self.pair.master_page.stage.0.epoch
    }

    pub(crate) fn page_attestations(&self) -> Option<(PageAttestation<'_>, PageAttestation<'_>)> {
        if !self.is_leased() {
            return None;
        }
        fn page<'a>(page: &'a Page, ordinal: usize) -> Option<PageAttestation<'a>> {
            if !page.invocation.matches_original(&page.original) {
                return None;
            }
            let context = &page.stage.0.context;
            let (request_binding, authority_binding, request) = page.original.attested_bindings(
                &context.owner,
                &context.database,
                "changes/zone",
            )?;
            Some(PageAttestation {
                invocation: page.invocation.id(),
                ordinal,
                record: page.records.get(ordinal)?,
                request,
                request_binding,
                authority_binding,
            })
        }
        Some((
            page(&self.pair.master_page, self.pair.master_ordinal)?,
            page(&self.pair.child_page, self.pair.child_ordinal)?,
        ))
    }
    pub(crate) fn page_invocations(
        &self,
    ) -> (
        &super::session::AcceptedInvocation,
        &super::session::AcceptedInvocation,
    ) {
        (
            &self.pair.master_page.invocation,
            &self.pair.child_page.invocation,
        )
    }
    pub(crate) fn source(
        &self,
    ) -> (
        &AccountOwner,
        &Value,
        &str,
        &RouteWitness,
        &OriginalBody,
        usize,
        &OriginalBody,
        usize,
    ) {
        let p = &self.pair;
        (
            &p.master_page.stage.0.context.owner,
            &p.master_page.stage.0.context.zone,
            &self.pass,
            &self.witness,
            &p.master_page.original,
            p.master_ordinal,
            &p.child_page.original,
            p.child_ordinal,
        )
    }
    pub(crate) fn records(&self) -> Option<(&Value, &Value)> {
        Some((
            self.pair
                .master_page
                .records
                .get(self.pair.master_ordinal)?,
            self.pair.child_page.records.get(self.pair.child_ordinal)?,
        ))
    }
}

impl RoutedObservation {
    /// The state store must enforce durable uniqueness even when a cancelled
    /// background commit races an attempt to mint a new candidate.
    pub(super) fn candidate(
        &self,
        lease_active: Arc<std::sync::atomic::AtomicBool>,
    ) -> CompactCandidate {
        CompactCandidate {
            pair: Arc::clone(&self.pair),
            pass: self.pass.clone(),
            witness: self.witness.clone(),
            lease_active,
        }
    }
}

impl std::fmt::Debug for RoutedObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted routed observation>")
    }
}

impl Drop for RoutedObservation {
    fn drop(&mut self) {
        // A dropped/cancelled uncommitted route must be obtainable again.
        // Durable committed uniqueness is the state transaction's job.
        if let Ok(mut routes) = self.pair.routed.lock() {
            routes.remove(&self.pass);
        }
    }
}

impl ZoneStage {
    pub(crate) fn new(context: AuthenticatedZoneScope, passes: HashSet<String>) -> Option<Self> {
        if context.zone.get("zoneName")?.as_str()?.trim().is_empty() || passes.is_empty() {
            return None;
        }
        Some(Self(Arc::new(Stage {
            #[cfg(test)]
            invalid_ordinal: std::sync::atomic::AtomicBool::new(false),
            epoch: uuid::Uuid::new_v4(),
            context,
            passes,
            pairs: Mutex::new(HashSet::new()),
        })))
    }
    pub(crate) fn context(&self) -> &AuthenticatedZoneScope {
        &self.0.context
    }
    #[cfg(test)]
    pub(crate) fn inject_invalid_ordinal(&self) {
        self.0
            .invalid_ordinal
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Mint before the actual request, binding its exact immutable input.
    pub(crate) fn request(&self, body: String, headers: &[(&str, &str)]) -> Option<PageRequest> {
        let parsed = super::changes_json::parse(body.as_bytes()).ok()?;
        let zones = parsed.get("zones")?.as_array()?;
        if zones.len() != 1 || zones.first()?.get("zoneID")? != &self.0.context.zone {
            return None;
        }
        Some(PageRequest {
            stage: self.clone(),
            body,
            headers: headers
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
        })
    }
}

impl PageRequest {
    /// Execute this one-shot request itself. The original raw ingestion body is
    /// returned regardless of optional proof, and the seal never leaves this
    /// invocation for a caller to swap with an identical concurrent request.
    /// Durable state receipt integration is still required before use.
    pub(crate) async fn execute(
        self,
        session: &dyn super::session::PhotosSession,
        url: &str,
        database: &str,
        retry: &crate::retry::RetryConfig,
    ) -> anyhow::Result<(Vec<u8>, Option<Vec<Record>>)> {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let mut response = super::session::retry_post_changes_with_evidence(
            session, url, &self.body, &headers, retry,
        )
        .await?;
        let original = response
            .original
            .take()
            .filter(|original| original.as_bytes() == response.body.as_slice());
        let accepted = response.accepted.take();
        let records = match original.zip(accepted) {
            Some((original, accepted)) => {
                self.accept(original, accepted, database).map_err(|error| {
                    super::error::ShadowPageError::from(anyhow::Error::new(OrdinalRefusal(error)))
                })?
            }
            None => None,
        };
        Ok((response.body, records))
    }

    /// Consumes the request capability. The raw response must be the very same
    /// request's authenticated source, and the returned zone must agree.
    fn accept(
        self,
        body: OriginalBody,
        accepted: super::session::AcceptedInvocation,
        database: &str,
    ) -> Result<Option<Vec<Record>>, InvalidSourceOrdinal> {
        let Some((typed, page)) = self.prepare_page(body, accepted, database) else {
            return Ok(None);
        };
        typed
            .into_iter()
            .enumerate()
            .map(|(ordinal, mut record)| {
                #[cfg(test)]
                let ordinal = if page
                    .stage
                    .0
                    .invalid_ordinal
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    usize::MAX
                } else {
                    ordinal
                };
                record
                    .attach_source_ordinal(SourceOrdinal {
                        page: Arc::clone(&page),
                        ordinal,
                    })
                    .ok_or(InvalidSourceOrdinal)?;
                Ok(record)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    fn prepare_page(
        self,
        body: OriginalBody,
        accepted: super::session::AcceptedInvocation,
        database: &str,
    ) -> Option<(Vec<Record>, Arc<Page>)> {
        if !accepted.matches_original(&body) {
            return None;
        }
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        if !body.matches_authenticated_source(
            &self.stage.0.context.owner,
            database,
            "changes/zone",
            &self.body,
            &headers,
        ) {
            return None;
        }
        let parsed = super::changes_json::parse(body.as_bytes()).ok()?;
        let zones = parsed.get("zones")?.as_array()?;
        if zones.len() != 1 {
            return None;
        }
        let zone = zones.first()?;
        if !response_zone_matches(zone.get("zoneID")?, &self.stage.0.context.zone) {
            return None;
        }
        let records = zone.get("records")?.as_array()?.clone();
        let typed: Vec<Record> = records
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()
            .ok()?;
        let page = Arc::new(Page {
            id: uuid::Uuid::new_v4(),
            stage: self.stage,
            original: body,
            invocation: accepted,
            records,
        });
        Some((typed, page))
    }
}

impl PairedObservation {
    /// Called while the actual DeltaRecordBuffer pair is being converted.
    pub(crate) fn pair(master: &Record, child: &Record) -> Option<Arc<Self>> {
        let master_origin = master.source_ordinal()?;
        let child_origin = child.source_ordinal()?;
        if !Arc::ptr_eq(&master_origin.page.stage.0, &child_origin.page.stage.0)
            || master.record_type != "CPLMaster"
            || child.record_type != "CPLAsset"
            || master.observed_value()? != master_origin.page.records.get(master_origin.ordinal)?
            || child.observed_value()? != child_origin.page.records.get(child_origin.ordinal)?
            || child
                .fields
                .pointer("/masterRef/value/recordName")?
                .as_str()?
                != master.record_name
        {
            return None;
        }
        let key = (
            master_origin.page.id,
            master_origin.ordinal,
            child_origin.page.id,
            child_origin.ordinal,
        );
        let mut issued = master_origin.page.stage.0.pairs.lock().ok()?;
        if issued.len() >= 4096 || !issued.insert(key) {
            return None;
        }
        drop(issued);
        Some(Arc::new(Self {
            master_page: Arc::clone(&master_origin.page),
            master_ordinal: master_origin.ordinal,
            child_page: Arc::clone(&child_origin.page),
            child_ordinal: child_origin.ordinal,
            routed: Mutex::new(HashSet::new()),
        }))
    }

    /// Claims a distinct pass once. The caller must invoke this only for the
    /// actual routed asset; state must independently validate the witness.
    pub(crate) fn route(
        self: &Arc<Self>,
        pass: &str,
        witness: RouteWitness,
    ) -> Option<RoutedObservation> {
        if !self.master_page.stage.0.passes.contains(pass)
            || matches!(&witness, RouteWitness::Selector(value) if value.is_null())
        {
            return None;
        }
        let mut routed = self.routed.lock().ok()?;
        if !routed.insert(pass.to_owned()) {
            return None;
        }
        Some(RoutedObservation {
            pair: Arc::clone(self),
            pass: pass.to_owned(),
            witness,
        })
    }
}

impl RoutedObservation {
    pub(crate) fn stage_epoch(&self) -> uuid::Uuid {
        self.pair.master_page.stage.0.epoch
    }

    /// These references are borrowed from actual records, not reconstructed
    /// from IDs. State must check both invocation/body pairs and persist a
    /// unique owner/root/pass/ordinal attestation atomically with admission.
    pub(crate) fn page_invocations(
        &self,
    ) -> (
        &super::session::AcceptedInvocation,
        &super::session::AcceptedInvocation,
    ) {
        (
            &self.pair.master_page.invocation,
            &self.pair.child_page.invocation,
        )
    }

    /// Read-only access for a state transaction. It must validate both pages,
    /// ordinals, owner, zone, membership, and its new generation atomically.
    pub(crate) fn source(
        &self,
    ) -> (
        &AccountOwner,
        &Value,
        &str,
        &RouteWitness,
        &OriginalBody,
        usize,
        &OriginalBody,
        usize,
    ) {
        let p = &self.pair;
        (
            &p.master_page.stage.0.context.owner,
            &p.master_page.stage.0.context.zone,
            &self.pass,
            &self.witness,
            &p.master_page.original,
            p.master_ordinal,
            &p.child_page.original,
            p.child_ordinal,
        )
    }

    pub(crate) fn records(&self) -> Option<(&Value, &Value)> {
        Some((
            self.pair
                .master_page
                .records
                .get(self.pair.master_ordinal)?,
            self.pair.child_page.records.get(self.pair.child_ordinal)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_zone_requires_owner_and_rejects_conflicting_extra_fields() {
        let request =
            json!({"zoneName":"z","ownerRecordName":"owner","zoneType":"REGULAR_CUSTOM_ZONE"});
        assert!(response_zone_matches(
            &json!({"zoneName":"z","ownerRecordName":"owner"}),
            &request
        ));
        assert!(response_zone_matches(&request, &request));
        assert!(!response_zone_matches(
            &json!({"zoneName":"z","ownerRecordName":"other"}),
            &request
        ));
        assert!(!response_zone_matches(&json!({"zoneName":"z"}), &request));
        assert!(!response_zone_matches(
            &json!({"zoneName":"z","ownerRecordName":"owner","zoneType":"other"}),
            &request
        ));
        assert!(!response_zone_matches(
            &json!({"zoneName":"z","ownerRecordName":"owner","unexpected":"x"}),
            &request
        ));
    }

    #[test]
    fn source_scope_rejects_missing_owner_or_zone_name() {
        let owner = owner();
        let authority =
            super::super::session::SourceAuthority::synthetic(&owner, "https://source.invalid")
                .unwrap();
        for zone in [
            json!({"zoneName":"z"}),
            json!({"ownerRecordName":"owner"}),
            json!({"zoneName":"z","ownerRecordName":""}),
        ] {
            let scope = json!({"database":"private","zone":zone}).to_string();
            assert!(
                AuthenticatedZoneScope::from_album(
                    owner.clone(),
                    scope,
                    zone,
                    "private".to_owned(),
                    authority.clone()
                )
                .is_none()
            );
        }
        assert!(!response_zone_matches(
            &json!({"zoneName":"z"}),
            &json!({"zoneName":"z"})
        ));
        assert!(!response_zone_matches(
            &json!({"ownerRecordName":"o"}),
            &json!({"ownerRecordName":"o"})
        ));
    }

    fn owner() -> AccountOwner {
        AccountOwner::authenticated(
            "stage@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-stage"}})).unwrap(),
        )
        .unwrap()
    }

    fn make_stage(owner: AccountOwner, zone: Value, passes: HashSet<String>) -> ZoneStage {
        let scope = json!({"database":"private", "zone":zone}).to_string();
        let authority =
            super::super::session::SourceAuthority::synthetic(&owner, "https://source.invalid")
                .unwrap();
        let context =
            AuthenticatedZoneScope::from_album(owner, scope, zone, "private".to_owned(), authority)
                .unwrap();
        ZoneStage::new(context, passes).unwrap()
    }

    fn record_result(
        stage: &ZoneStage,
        item: Value,
    ) -> Option<Result<Option<Vec<Record>>, InvalidSourceOrdinal>> {
        let headers = [("Content-type", "text/plain")];
        let request =
            json!({"zones":[{"zoneID": stage.0.context.zone, "syncToken":"synthetic-cursor", "resultsLimit":200}]})
                .to_string();
        let url = "https://source.invalid/database/1/com.apple.photos.cloud/production/private/changes/zone";
        let response = serde_json::to_vec(
            &json!({"zones":[{"zoneID":stage.0.context.zone,"records":[item]}]}),
        )
        .ok()?;
        let raw = super::super::session::OriginalBody::synthetic_source(
            &stage.0.context.owner,
            "https://source.invalid",
            url,
            &request,
            &headers,
            &response,
        )?;
        let (raw, accepted) = raw.synthetic_accepted();
        Some(
            stage
                .request(request, &headers)?
                .accept(raw, accepted, "private"),
        )
    }

    fn record(stage: &ZoneStage, item: Value) -> Record {
        record_result(stage, item)
            .unwrap()
            .unwrap()
            .unwrap()
            .pop()
            .unwrap()
    }

    #[test]
    fn ordinal_refusal_retains_its_typed_cause_through_shadow_error() {
        let error = anyhow::Error::new(super::super::error::ShadowPageError::from(
            anyhow::Error::new(OrdinalRefusal(InvalidSourceOrdinal)),
        ));
        assert!(
            error
                .downcast_ref::<super::super::error::ShadowPageError>()
                .is_some()
        );
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<InvalidSourceOrdinal>())
        );
    }

    #[test]
    fn invalid_ordinal_is_a_typed_acceptance_error() {
        let stage = make_stage(
            owner(),
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            ["pass".to_owned()].into_iter().collect(),
        );
        stage.inject_invalid_ordinal();
        let result = record_result(
            &stage,
            json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
        )
        .unwrap();
        assert!(matches!(result, Err(InvalidSourceOrdinal)));
        assert!(
            record_result(
                &stage,
                json!({"recordName":"m2","recordType":"CPLMaster","fields":{}})
            )
            .unwrap()
            .unwrap()
            .is_some()
        );
    }

    #[tokio::test]
    async fn mismatched_seal_and_ingestion_body_cannot_create_records() {
        struct SwappingSession {
            owner: AccountOwner,
            identical: bool,
            saved: Arc<Mutex<Option<OriginalBody>>>,
        }
        #[async_trait::async_trait]
        impl super::super::session::PhotosSession for SwappingSession {
            async fn post(&self, _: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
                panic!("raw path expected")
            }
            async fn post_changes_with_evidence(
                &self,
                url: &str,
                body: String,
                headers: &[(&str, &str)],
                invocation: super::super::session::InvocationToken,
            ) -> anyhow::Result<super::super::session::ChangesResponse> {
                let first = br#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":[]}]}"#;
                let second = br#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":[{"recordName":"other","recordType":"CPLMaster"}]}]}"#;
                let mut response = super::super::session::ChangesResponse::synthetic(
                    &self.owner,
                    "https://source.invalid",
                    url,
                    &body,
                    headers,
                    first,
                    invocation,
                )?;
                if self.identical {
                    let mut saved = self.saved.lock().unwrap();
                    let previous = std::mem::replace(&mut *saved, response.original.take());
                    response.original = previous;
                } else {
                    response.body = second.to_vec();
                }
                Ok(response)
            }
            fn clone_box(&self) -> Box<dyn super::super::session::PhotosSession> {
                Box::new(Self {
                    owner: self.owner.clone(),
                    identical: self.identical,
                    saved: Arc::clone(&self.saved),
                })
            }
        }
        let owner = owner();
        let stage = make_stage(
            owner.clone(),
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            ["pass".to_owned()].into_iter().collect(),
        );
        let request = json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"cursor"}]})
            .to_string();
        let retry = crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let (body, records) = stage.request(request, &[]).unwrap().execute(&SwappingSession { owner: owner.clone(), identical: false, saved: Arc::new(Mutex::new(None)) }, "https://source.invalid/database/1/com.apple.photos.cloud/production/private/changes/zone", "private", &retry).await.unwrap();
        assert!(!body.is_empty());
        assert!(records.is_none());
        // First invocation caches its seal; the second returns that old seal
        // with byte-identical response. It must not gain the new invocation.
        let stale = SwappingSession {
            owner,
            identical: true,
            saved: Arc::new(Mutex::new(None)),
        };
        for _ in 0..2 {
            let request =
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"cursor"}]})
                    .to_string();
            let (_, records) = stage.request(request, &[]).unwrap().execute(
                &stale, "https://source.invalid/database/1/com.apple.photos.cloud/production/private/changes/zone", "private", &retry,
            ).await.unwrap();
            assert!(records.is_none());
        }
    }

    #[test]
    fn invalid_source_ordinal_never_attaches_or_attests_source() {
        for invalid_master in [false, true] {
            let stage = make_stage(
                owner(),
                json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ["pass".to_owned()].into_iter().collect(),
            );
            let mut master = record(
                &stage,
                json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
            );
            let child = record(
                &stage,
                json!({"recordName":"c","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"m"}}}}),
            );
            let mut invalid = master.source_ordinal().unwrap().clone();
            invalid.ordinal = usize::MAX;
            assert!(master.attach_source_ordinal(invalid).is_none());
            assert!(master.source_ordinal().is_none());
            assert!(master.observed_value().is_none());
            assert!(PairedObservation::pair(&master, &child).is_none());

            let stage = make_stage(
                owner(),
                json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ["pass".to_owned()].into_iter().collect(),
            );
            let master = record(
                &stage,
                json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
            );
            let child = record(
                &stage,
                json!({"recordName":"c","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"m"}}}}),
            );
            let mut pair = PairedObservation::pair(&master, &child).unwrap();
            let pair_mut = Arc::get_mut(&mut pair).unwrap();
            if invalid_master {
                pair_mut.master_ordinal = usize::MAX;
            } else {
                pair_mut.child_ordinal = usize::MAX;
            }
            let route = pair.route("pass", RouteWitness::DirectZone).unwrap();
            assert!(route.records().is_none());
            let candidate = route.candidate(Arc::new(std::sync::atomic::AtomicBool::new(true)));
            assert!(candidate.records().is_none());
            assert!(candidate.page_attestations().is_none());
        }
    }

    #[test]
    fn actual_pair_survives_pages_and_route_is_once_per_pass() {
        let stage = make_stage(
            owner(),
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            ["pass-a".to_owned(), "pass-b".to_owned()]
                .into_iter()
                .collect(),
        );
        let master = record(
            &stage,
            json!({"recordName":"m","recordType":"CPLMaster", "fields":{}}),
        );
        let child = record(
            &stage,
            json!({"recordName":"c","recordType":"CPLAsset",
            "fields":{"masterRef":{"value":{"recordName":"m"}},"assetDate":{"value":1736899200000_i64}}}),
        );
        let pair = PairedObservation::pair(&master, &child).unwrap();
        assert!(PairedObservation::pair(&master, &child).is_none());
        let route = pair.route("pass-a", RouteWitness::DirectZone).unwrap();
        let (master_record, child_record) = route.records().unwrap();
        assert_eq!(master_record["recordName"], "m");
        assert_eq!(child_record["recordName"], "c");
        assert!(pair.route("pass-a", RouteWitness::DirectZone).is_none());
        drop(route);
        assert!(pair.route("pass-a", RouteWitness::DirectZone).is_some());
        assert!(
            pair.route("pass-b", RouteWitness::Selector(json!({"synthetic":true})))
                .is_some()
        );
        assert!(pair.route("other", RouteWitness::DirectZone).is_none());
        assert!(
            pair.route("pass-b", RouteWitness::Selector(Value::Null))
                .is_none()
        );
    }

    #[test]
    fn candidate_preserves_actual_scope_for_same_owner_and_zone() {
        let owner = owner();
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let authority =
            super::super::session::SourceAuthority::synthetic(&owner, "https://source.invalid")
                .unwrap();
        let make_scoped = |realm: &str| {
            let scope = json!({"database":"private", "zone":zone, "realm":realm}).to_string();
            let context = AuthenticatedZoneScope::from_album(
                owner.clone(),
                scope,
                zone.clone(),
                "private".to_owned(),
                authority.clone(),
            )
            .unwrap();
            ZoneStage::new(context, ["pass".to_owned()].into_iter().collect()).unwrap()
        };
        let first = make_scoped("first");
        let second = make_scoped("second");
        let pair_for = |stage: &ZoneStage| {
            let master = record(
                stage,
                json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
            );
            let child = record(
                stage,
                json!({"recordName":"c","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"m"}},"assetDate":{"value":1736899200000_i64}}}),
            );
            PairedObservation::pair(&master, &child).unwrap()
        };
        let one = pair_for(&first)
            .route("pass", RouteWitness::DirectZone)
            .unwrap();
        let two = pair_for(&second)
            .route("pass", RouteWitness::DirectZone)
            .unwrap();
        let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let one = one.candidate(active.clone());
        let two = two.candidate(active);
        assert_eq!(one.source_scope(), first.0.context.scope());
        assert_eq!(two.source_scope(), second.0.context.scope());
        assert_ne!(one.source_scope(), two.source_scope());
    }

    #[tokio::test]
    async fn routed_capability_survives_borrowed_rollback() {
        let stage = make_stage(
            owner(),
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            ["pass".to_owned()].into_iter().collect(),
        );
        let master = record(
            &stage,
            json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
        );
        let child = record(
            &stage,
            json!({"recordName":"c","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"m"}}}}),
        );
        let asset = super::super::asset::PhotoAsset::from_records(master, &child)
            .with_direct_zone_route("pass");
        let candidate = {
            let borrowed = asset.lock_causal_route().await.unwrap();
            assert!(borrowed.observation().is_some());
            let candidate = borrowed.candidate().unwrap();
            assert!(candidate.is_leased());
            candidate
            // Dropping this uncommitted lease invalidates its candidate.
        };
        assert!(!candidate.is_leased());
        assert!(
            asset
                .lock_causal_route()
                .await
                .unwrap()
                .observation()
                .is_some()
        );
        // A future state integration must require a committed receipt before
        // offering a consuming operation; no such operation exists yet.
    }

    #[tokio::test]
    async fn dropping_routed_asset_releases_uncommitted_pass_reservation() {
        let stage = make_stage(
            owner(),
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            ["pass".to_owned()].into_iter().collect(),
        );
        let master = record(
            &stage,
            json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
        );
        let child = record(
            &stage,
            json!({"recordName":"c","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"m"}}}}),
        );
        let asset = super::super::asset::PhotoAsset::from_records(master, &child);
        let routed = asset.clone().with_direct_zone_route("pass");
        assert!(
            routed
                .lock_causal_route()
                .await
                .unwrap()
                .observation()
                .is_some()
        );
        drop(routed);
        let again = asset.with_direct_zone_route("pass");
        assert!(
            again
                .lock_causal_route()
                .await
                .unwrap()
                .observation()
                .is_some()
        );
    }

    #[test]
    fn saturated_pair_registry_keeps_ordinary_asset_available() {
        let stage = make_stage(
            owner(),
            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            ["pass".to_owned()].into_iter().collect(),
        );
        {
            let mut pairs = stage.0.pairs.lock().unwrap();
            for _ in 0..4096 {
                pairs.insert((uuid::Uuid::new_v4(), 0, uuid::Uuid::new_v4(), 0));
            }
        }
        let master = record(
            &stage,
            json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
        );
        let child = record(
            &stage,
            json!({"recordName":"c","recordType":"CPLAsset","fields":{"masterRef":{"value":{"recordName":"m"}}}}),
        );
        assert!(PairedObservation::pair(&master, &child).is_none());
        let asset = super::super::asset::PhotoAsset::from_records(master, &child);
        assert!(asset.causal_pair().is_none());
        assert!(asset.observed_pair().is_some());
    }

    #[test]
    fn identical_records_from_distinct_observations_do_not_pair() {
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let passes = ["pass".to_owned()].into_iter().collect();
        let first = make_stage(owner(), zone.clone(), passes);
        let second = make_stage(owner(), zone, ["pass".to_owned()].into_iter().collect());
        let master = record(
            &first,
            json!({"recordName":"m","recordType":"CPLMaster","fields":{}}),
        );
        let child = record(
            &second,
            json!({"recordName":"c","recordType":"CPLAsset",
            "fields":{"masterRef":{"value":{"recordName":"m"}}}}),
        );
        assert!(PairedObservation::pair(&master, &child).is_none());
    }
}
