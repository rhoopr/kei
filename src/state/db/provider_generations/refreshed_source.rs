//! Narrow validation of a retained, trusted-runtime lookup attestation.
//! This module does not authenticate a transport or authorize dispatch: only
//! an already-claimed, one-shot provider witness may create an attestation.
use crate::icloud::photos::{changes_json, current_asset, queries, session::SourceAuthority};
use crate::state::VersionSizeKey;
use crate::state::db::account::AccountOwner;
use serde_json::{Value, json};

use super::MAX_CHUNK_BYTES;

/// Recheck request scope, pair structure, tags, exact resource and current
/// authenticated endpoint. The binding and invocation identify the original
/// trusted-runtime observation; they do not prove ownership alone.
pub(super) fn validate_retained_lookup(
    owner: &AccountOwner,
    authority: &SourceAuthority,
    database: &str,
    zone: &Value,
    child: &str,
    master: &str,
    version: VersionSizeKey,
    checksum: &str,
    size: u64,
    url: &str,
    request_body: &[u8],
    response_body: &[u8],
    request_binding: &[u8; 32],
    authority_binding: &[u8; 32],
    invocation: uuid::Uuid,
) -> bool {
    if request_body.len() > MAX_CHUNK_BYTES
        || response_body.len() > MAX_CHUNK_BYTES
        || child.is_empty()
        || master.is_empty()
        || child == master
        || request_binding.iter().all(|v| *v == 0)
        || invocation.is_nil()
        || authority.binding(owner, database, "records/lookup") != Some(*authority_binding)
    {
        return false;
    }
    let Ok(request) = changes_json::parse(request_body) else {
        return false;
    };
    if request
        != json!({
            "records": [{"recordName": child}, {"recordName": master}],
            "zoneID": zone,
            "desiredKeys": &*queries::IDENTITY_LOOKUP_KEYS_VALUES,
        })
    {
        return false;
    }
    let Ok(response) = changes_json::parse(response_body) else {
        return false;
    };
    let Some(records) = response.get("records").and_then(Value::as_array) else {
        return false;
    };
    if records.len() != 2
        || [child, master].into_iter().any(|name| {
            let mut matches = records
                .iter()
                .filter(|record| record.get("recordName").and_then(Value::as_str) == Some(name));
            let valid = matches.next().is_some_and(|record| {
                record
                    .get("recordChangeTag")
                    .and_then(Value::as_str)
                    .is_some_and(|tag| !tag.trim().is_empty())
            });
            !valid || matches.next().is_some()
        })
    {
        return false;
    }
    let Ok(asset) = current_asset(response_body, child, master, zone) else {
        return false;
    };
    let mut matching = asset.versions().iter().filter(|(candidate, resource)| {
        VersionSizeKey::from(*candidate) == version
            && resource.checksum.as_ref() == checksum
            && resource.size == size
    });
    let Some((_, resource)) = matching.next() else {
        return false;
    };
    resource.url.as_ref() == url && matching.next().is_none()
}

/// Compute the existing immutable-epoch key for the *authenticated fresh
/// confirmation*, while retaining and checking the original admitted task.
/// This locator is a hint only: caller must consume the live witness and
/// re-read/revalidate the complete candidate set before dispatch.
pub(super) fn original_epoch_locator(
    conn: &rusqlite::Connection,
    root: &super::ActiveGeneration,
    decision: &super::ActiveDecision,
    original: &super::transfer_wait::SelectionTransferKey,
    policy: &crate::download::CurrentPolicyEligibility,
    fresh_confirmation: &[u8],
    fresh_url: &str,
) -> Result<Option<String>, crate::state::error::StateError> {
    if conn.is_autocommit()
        || root.id != original.generation
        || !super::transfer_wait::matches_task(conn, root, decision, original)?
    {
        return Ok(None);
    }
    let Some(saved_confirmation) = decision.decision.confirmation.as_deref() else {
        return Ok(None);
    };
    if !policy.matches(
        root,
        &original.pass_key,
        &original.child,
        &original.master,
        saved_confirmation,
    ) {
        return Ok(None);
    }
    original_epoch_locator_after_identity(
        conn,
        root,
        original,
        policy,
        fresh_confirmation,
        fresh_url,
    )
}

fn original_epoch_locator_after_identity(
    conn: &rusqlite::Connection,
    root: &super::ActiveGeneration,
    original: &super::transfer_wait::SelectionTransferKey,
    policy: &crate::download::CurrentPolicyEligibility,
    fresh_confirmation: &[u8],
    fresh_url: &str,
) -> Result<Option<String>, crate::state::error::StateError> {
    let (account, provider) = super::owner_keys(conn)?;
    let path = original.path.key()?;
    Ok(Some(super::digest(&super::encode(&(
        "expired-transfer-obligation-v1",
        (
            account,
            provider,
            &root.spec.scope,
            &root.spec.zone,
            &root.spec.config_hash,
            &root.spec.profile,
            root.spec.metadata_enabled,
            root.spec.metadata_flags,
            super::digest(policy.resolved_snapshot()),
            super::digest(fresh_confirmation),
        ),
        (
            &original.pass_key,
            &original.child,
            &original.state_id,
            &original.master,
            &original.version_size,
            path,
            &original.checksum,
            original.size,
            fresh_url,
        ),
    ))?)))
}

/// Validate an already verified selected destination without treating a
/// completed receipt as pending. The caller must also validate the actual
/// authenticated download receipt and held file.
pub(super) fn completed_epoch_locator(
    conn: &rusqlite::Connection,
    root: &super::ActiveGeneration,
    decision: &super::ActiveDecision,
    original: &super::transfer_wait::SelectionTransferKey,
    policy: &crate::download::CurrentPolicyEligibility,
    fresh_confirmation: &[u8],
    fresh_url: &str,
) -> Result<Option<String>, crate::state::error::StateError> {
    use rusqlite::params;
    if conn.is_autocommit()
        || root.spec.format != 2
        || root.id != original.generation
        || decision.state_id != original.state_id
        || decision.decision.pass_key != original.pass_key
        || decision.decision.child != original.child
        || decision.decision.master.as_deref() != Some(original.master.as_str())
    {
        return Ok(None);
    }
    let Some(destination) = decision.decision.destinations.iter().find(|d| {
        d.version_size == original.version_size
            && d.path == original.path
            && d.checksum == original.checksum
            && d.size == original.size
    }) else {
        return Ok(None);
    };
    let path = destination.path.key()?;
    let verified:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND admitted=1 AND verified_media=1 AND length(local_checksum)=64 AND length(source_checksum)=64)",
        params![root.id,original.pass_key,original.child,original.version_size,path],|r|r.get(0))?;
    if !verified {
        return Ok(None);
    }
    let Some(body) = decision.decision.confirmation.as_deref() else {
        return Ok(None);
    };
    if !policy.matches(
        root,
        &original.pass_key,
        &original.child,
        &original.master,
        body,
    ) {
        return Ok(None);
    }
    let asset = current_asset(body, &original.child, &original.master, &root.spec.zone)
        .map_err(|_redacted_error| crate::state::error::StateError::ProviderSelectionInvalid)?;
    let logical = VersionSizeKey::from_str(&original.version_size)
        .ok_or(crate::state::error::StateError::ProviderSelectionInvalid)?;
    if !asset.versions().iter().any(|(version, resource)| {
        let provider = VersionSizeKey::from(*version);
        (provider == logical
            || matches!(
                (provider, logical),
                (VersionSizeKey::Original, VersionSizeKey::Alternative)
                    | (VersionSizeKey::Alternative, VersionSizeKey::Original)
            ))
            && resource.url.as_ref() == original.url
            && resource.checksum.as_ref() == original.checksum
            && resource.size == original.size
    }) {
        return Ok(None);
    }
    original_epoch_locator_after_identity(
        conn,
        root,
        original,
        policy,
        fresh_confirmation,
        fresh_url,
    )
}

/// Final same-transaction validation of one existing immutable 410 owner.
/// The fresh confirmation must have come from the claimed live witness.
/// Both policy capabilities must be minted from their own actual roots.
#[allow(clippy::too_many_arguments)]
pub(super) fn original_epoch_waiting_tx(
    conn: &rusqlite::Connection,
    owner: &AccountOwner,
    authority: &SourceAuthority,
    original: &super::transfer_wait::SelectionTransferKey,
    target_route: &str,
    target_policy: &crate::download::CurrentPolicyEligibility,
    fresh_confirmation: &[u8],
    fresh_url: &str,
    hint: &super::immutable_epochs::EpochHint,
    source_policy: &crate::download::CurrentPolicyEligibility,
) -> Result<bool, crate::state::error::StateError> {
    use crate::state::error::StateError;
    use rusqlite::{OptionalExtension, params};
    if conn.is_autocommit() {
        return Err(StateError::ProviderSelectionInvalid);
    }
    super::account::validate_authenticated(conn, owner)?;
    let target_root = super::load_generation(conn, &original.generation)?;
    let target_decision =
        super::load_decision(conn, &target_root, &original.pass_key, &original.child)?;
    let Some(locator) = original_epoch_locator(
        conn,
        &target_root,
        &target_decision,
        original,
        target_policy,
        fresh_confirmation,
        fresh_url,
    )?
    else {
        return Ok(false);
    };
    if hint.pass_key != original.pass_key
        || hint.child != original.child
        || hint.version_size != original.version_size
        || hint.path != original.path.key()?
        || target_policy.policy_preimage() != source_policy.policy_preimage()
        || target_policy.resolved_snapshot() != source_policy.resolved_snapshot()
    {
        return Ok(false);
    }
    let root = super::load_generation(conn, &hint.generation)?;
    let decision = super::load_decision(conn, &root, &hint.pass_key, &hint.child)?;
    if decision.decision.confirmation.as_deref() != Some(fresh_confirmation) {
        return Ok(false);
    }
    let mut source = original.clone();
    source.generation = hint.generation.clone();
    source.url = fresh_url.to_owned();
    if super::immutable_epochs::exact_locator(conn, &root, &decision, &source, source_policy)?
        .as_deref()
        != Some(locator.as_str())
    {
        return Ok(false);
    }
    let marker = super::digest(&super::encode(&(
        "immutable-expired-transfer-410-v1",
        &source,
        hint.attempt,
        hint.deadline,
        &hint.route_id,
        &locator,
    ))?);
    if marker != hint.marker_hash {
        return Err(StateError::ProviderSelectionInvalid);
    }
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_410_failure_epochs WHERE obligation_digest=?1 AND deadline=?2 AND generation=?3 AND pass_key=?4 AND child=?5 AND version_size=?6 AND path=?7 AND attempt=?8 AND marker_hash=?9 AND route_id=?10 AND state_id=?11 AND master=?12 AND checksum=?13 AND size_bytes=?14 AND url=?15)",
        params![locator,hint.deadline,hint.generation,hint.pass_key,hint.child,hint.version_size,hint.path,hint.attempt,hint.marker_hash,hint.route_id,source.state_id,source.master,source.checksum,i64::try_from(source.size).map_err(|_redacted_error| StateError::ProviderSelectionInvalid)?,source.url],
        |r| r.get(0),
    )?;
    let epoch: Option<(i64,i64)> = conn.query_row(
        "SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",
        params![hint.generation,hint.pass_key,hint.child], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    if !present
        || epoch != Some((hint.attempt, hint.deadline))
        || hint.deadline <= super::super::retry_clock::now_for_connection(conn)
    {
        return Ok(false);
    }
    Ok(super::compact_attestation::validate_one_compact_receipt_tx(
        conn,
        owner,
        authority,
        &root.id,
        &hint.pass_key,
        &hint.child,
        &hint.route_id,
        source_policy,
    )? && super::compact_attestation::validate_one_compact_receipt_tx(
        conn,
        owner,
        authority,
        &target_root.id,
        &original.pass_key,
        &original.child,
        target_route,
        target_policy,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_scope_request_pair_and_resource_changes() {
        let owner = AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-refresh"}})).unwrap(),
        )
        .unwrap();
        let foreign = AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"foreign-refresh"}})).unwrap(),
        )
        .unwrap();
        let authority = SourceAuthority::synthetic(&owner, "https://source.invalid").unwrap();
        let binding = authority
            .binding(&owner, "private", "records/lookup")
            .unwrap();
        let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let request = serde_json::to_vec(&json!({
            "records":[{"recordName":"asset-master"},{"recordName":"master"}],
            "zoneID":zone, "desiredKeys":&*queries::IDENTITY_LOOKUP_KEYS_VALUES
        }))
        .unwrap();
        let records = crate::test_helpers::mock_photo_records_for_zone_with_filename(
            "master",
            "PrimarySync",
            "test.jpg",
        );
        let response = serde_json::to_vec(&json!({"records":records})).unwrap();
        let id = uuid::Uuid::new_v4();
        let check =
            |owner: &AccountOwner, zone: &Value, request: &[u8], response: &[u8], url: &str| {
                validate_retained_lookup(
                    owner,
                    &authority,
                    "private",
                    zone,
                    "asset-master",
                    "master",
                    VersionSizeKey::Original,
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                    1024,
                    url,
                    request,
                    response,
                    &[1; 32],
                    &binding,
                    id,
                )
            };
        let url = "https://p01.icloud-content.com/master.jpg";
        assert!(check(&owner, &zone, &request, &response, url));
        assert!(!check(&foreign, &zone, &request, &response, url));
        assert!(!check(
            &owner,
            &json!({"zoneName":"Other"}),
            &request,
            &response,
            url
        ));
        assert!(!check(
            &owner,
            &zone,
            &request,
            &response,
            "https://source.invalid/changed"
        ));
        assert!(!check(&owner, &zone, br#"{"records":[]}"#, &response, url));
        assert!(!check(&owner, &zone, &request, br#"{"records":[]}"#, url));
        assert!(!check(
            &owner,
            &zone,
            br#"{"records":[],"records":[]}"#,
            &response,
            url
        ));
    }
}
