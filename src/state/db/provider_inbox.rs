//! Additive observations; these receipts never authorize a legacy checkpoint.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::{SqliteStateDb, account};
use crate::state::error::StateError;
use crate::support::{StorageComponent, StorageOutcome};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    pub(crate) name: String,
    pub(crate) record_type: Option<String>,
    pub(crate) deleted: bool,
}

/// Constructed by the provider adapter only after complete page validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedPage {
    pub(crate) scope: String,
    pub(crate) request_cursor: String,
    pub(crate) successor: String,
    pub(crate) more_coming: bool,
    pub(crate) body: Vec<u8>,
    pub(crate) identities: Vec<SourceIdentity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CapturedPageId(pub(crate) i64);

/// A retained legacy source whose transport-varying bytes were discarded after
/// the migration proved exact masked equivalence with a canonical page.
///
/// `certificate` is the provider-owned canonical key, `masked_sha` is the
/// digest of the classifier's complete masked bytes, and `classifier_version`
/// binds both to the rule that produced them. None of these values is content
/// proof. `StoredPage::observation_hash` remains the historical original hash,
/// not a digest of the representative bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LegacyAlias {
    pub(crate) canonical_page_id: CapturedPageId,
    pub(crate) classifier_version: u8,
    pub(crate) masked_sha: String,
    pub(crate) certificate: String,
    /// Immutable binding over the original identity, both raw observation
    /// hashes, the canonical class key, the classifier version and the exact
    /// ordinal identity list. It is verified on every load.
    pub(crate) provenance_hash: String,
}

/// Distinguishes a raw retained observation from a typed historical alias.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PageProvenance {
    Raw,
    LegacyAlias(LegacyAlias),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredPage {
    pub(crate) id: CapturedPageId,
    pub(crate) account_key: String,
    pub(crate) provider_key: String,
    /// The exact hash observed when this durable identity was first admitted.
    /// For `Raw` this is the SHA-256 of `page.body`; for `LegacyAlias` it is
    /// the historical hash of discarded transport bytes and must never be
    /// compared with representative bytes.
    pub(crate) observation_hash: String,
    pub(crate) provenance: PageProvenance,
    pub(crate) page: ObservedPage,
}

fn stored_identities(conn: &Connection, page_id: i64) -> Result<Vec<SourceIdentity>, StateError> {
    let identities = conn.prepare(
        "SELECT ordinal,record_name,record_type,deleted FROM provider_shadow_records WHERE page_id=?1 ORDER BY ordinal",
    )?.query_map([page_id], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,
        row.get::<_,Option<String>>(2)?,row.get::<_,i64>(3)?)))?.collect::<Result<Vec<_>,_>>()?;
    for (expected, (ordinal, _, _, deleted)) in identities.iter().enumerate() {
        if i64::try_from(expected).ok() != Some(*ordinal) || !matches!(deleted, 0 | 1) {
            return Err(StateError::ProviderCatalogInvalid);
        }
    }
    Ok(identities
        .into_iter()
        .map(|(_, name, record_type, deleted)| SourceIdentity {
            name,
            record_type,
            deleted: deleted == 1,
        })
        .collect())
}

pub(super) fn load_page(
    conn: &Connection,
    id: CapturedPageId,
    max_body_bytes: usize,
) -> Result<StoredPage, StateError> {
    let body_len: i64 = conn.query_row(
        "SELECT length(body) FROM provider_shadow_pages WHERE id=?1",
        [id.0],
        |row| row.get(0),
    )?;
    if u64::try_from(body_len).map_or(true, |len| len > max_body_bytes as u64) {
        return Err(StateError::ProviderCatalogInvalid);
    }
    let (account_key, provider_key, scope, request_cursor, successor, more, observation_hash, body):
        (String,String,String,String,String,i64,String,Vec<u8>) = conn.query_row(
        "SELECT account_key,provider_key,scope,request_cursor,successor,more_coming,body_hash,body FROM provider_shadow_pages WHERE id=?1",
        [id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?)),
    )?;
    let owner: (String, String) = conn.query_row(
        "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if (&account_key, &provider_key) != (&owner.0, &owner.1) || !matches!(more, 0 | 1) {
        return Err(StateError::ProviderCatalogInvalid);
    }
    let alias: Option<(i64, i64, String, String, String)> = conn.query_row(
        "SELECT canonical_page_id,classifier_version,masked_sha,certificate,provenance_hash FROM provider_transport_aliases WHERE page_id=?1",
        [id.0],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).optional()?;
    let (provenance, page, observation_hash) = if let Some((
        canonical_id,
        version,
        masked_sha,
        certificate,
        provenance_hash,
    )) = alias
    {
        // The legacy row keeps only provenance. A nonempty body would mean the
        // discarded original was not actually removed and violates the alias
        // certificate.
        if !body.is_empty() || canonical_id == id.0 {
            return Err(StateError::ProviderCatalogInvalid);
        }
        // An alias target must itself be a raw representative. Reject chains
        // and mutual cycles before any recursive load, so malformed rows fail
        // promptly instead of recursing.
        let target_is_alias: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_transport_aliases WHERE page_id=?1)",
            [canonical_id],
            |row| row.get(0),
        )?;
        if target_is_alias {
            return Err(StateError::ProviderCatalogInvalid);
        }
        let canonical = load_page(conn, CapturedPageId(canonical_id), max_body_bytes)?;
        let identities = stored_identities(conn, id.0)?;
        if !matches!(canonical.provenance, PageProvenance::Raw)
            || (&canonical.account_key, &canonical.provider_key) != (&account_key, &provider_key)
            || !same_capture_metadata(
                &ObservedPage {
                    scope: scope.clone(),
                    request_cursor: request_cursor.clone(),
                    successor: successor.clone(),
                    more_coming: more == 1,
                    body: Vec::new(),
                    identities: identities.clone(),
                },
                &canonical.page,
            )
        {
            return Err(StateError::ProviderCatalogInvalid);
        }
        let classifier_version =
            u8::try_from(version).map_err(|_error| StateError::ProviderCatalogInvalid)?;
        let evidence = crate::icloud::photos::transport_evidence::capture_evidence(&canonical.page)
            .map_err(|_error| StateError::ProviderCatalogInvalid)?
            .ok_or(StateError::ProviderCatalogInvalid)?;
        let expected_masked = format!("{:x}", Sha256::digest(&evidence.masked));
        let expected_certificate = canonical_key(
            &canonical.account_key,
            &canonical.provider_key,
            &canonical.page,
            &evidence,
        )?;
        let expected_provenance = alias_provenance_hash(
            id.0,
            &observation_hash,
            canonical_id,
            &canonical.observation_hash,
            &certificate,
            classifier_version,
            &identities,
        )?;
        if evidence.version != classifier_version
            || masked_sha != expected_masked
            || certificate != expected_certificate
            || provenance_hash != expected_provenance
        {
            return Err(StateError::ProviderCatalogInvalid);
        }
        let indexed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_transport_canonical WHERE canonical_sha=?1 AND version=?2 AND page_id=?3)",
            params![certificate, i64::from(classifier_version), canonical_id],
            |row| row.get(0),
        )?;
        if !indexed {
            return Err(StateError::ProviderCatalogInvalid);
        }
        (
            PageProvenance::LegacyAlias(LegacyAlias {
                canonical_page_id: CapturedPageId(canonical_id),
                classifier_version,
                masked_sha,
                certificate,
                provenance_hash,
            }),
            canonical.page,
            observation_hash,
        )
    } else {
        if observation_hash != format!("{:x}", Sha256::digest(&body)) {
            return Err(StateError::ProviderCatalogInvalid);
        }
        (
            PageProvenance::Raw,
            ObservedPage {
                scope,
                request_cursor,
                successor,
                more_coming: more == 1,
                body,
                identities: stored_identities(conn, id.0)?,
            },
            observation_hash,
        )
    };
    Ok(StoredPage {
        id,
        account_key,
        provider_key,
        observation_hash,
        provenance,
        page,
    })
}

/// Unambiguous canonical identity: account, provider, exact capture scope,
/// continuation metadata, classifier version, and the masked body digest.
fn canonical_key(
    account_key: &str,
    provider_key: &str,
    page: &ObservedPage,
    evidence: &crate::icloud::photos::transport_evidence::TransportEvidence,
) -> Result<String, StateError> {
    let masked_sha = format!("{:x}", Sha256::digest(&evidence.masked));
    let tuple = serde_json::to_vec(&(
        "provider-transport-canonical",
        1u8,
        account_key,
        provider_key,
        &page.scope,
        &page.request_cursor,
        &page.successor,
        page.more_coming,
        evidence.version,
        masked_sha,
    ))
    .map_err(|_error| StateError::ProviderInboxInvalid)?;
    Ok(format!("{:x}", Sha256::digest(tuple)))
}

fn same_capture_metadata(stored: &ObservedPage, incoming: &ObservedPage) -> bool {
    stored.scope == incoming.scope
        && stored.request_cursor == incoming.request_cursor
        && stored.successor == incoming.successor
        && stored.more_coming == incoming.more_coming
        && stored.identities == incoming.identities
}

/// Immutable provenance binding for one typed alias. The class key stays a
/// separate lookup value; this hash additionally binds the original durable
/// identity and historical hash to the canonical identity and both identity
/// lists, so neither side can be rewritten independently.
fn alias_provenance_hash(
    original_id: i64,
    original_hash: &str,
    canonical_id: i64,
    canonical_hash: &str,
    certificate: &str,
    version: u8,
    identities: &[SourceIdentity],
) -> Result<String, StateError> {
    let identities: Vec<(usize, &str, Option<&str>, bool)> = identities
        .iter()
        .enumerate()
        .map(|(ordinal, identity)| {
            (
                ordinal,
                identity.name.as_str(),
                identity.record_type.as_deref(),
                identity.deleted,
            )
        })
        .collect();
    let tuple = serde_json::to_vec(&(
        "provider-transport-alias-provenance",
        1u8,
        original_id,
        original_hash,
        canonical_id,
        canonical_hash,
        certificate,
        version,
        identities,
    ))
    .map_err(|_error| StateError::ProviderInboxInvalid)?;
    Ok(format!("{:x}", Sha256::digest(tuple)))
}

/// Budget charge retained for one typed legacy alias: canonical certificate,
/// masked digest, classifier version, provenance binding, and the alias
/// table/index row.
const ALIAS_OVERHEAD_BYTES: u64 = 320;
/// Canonical index row charge when migration must create one for a retained
/// representative. New captures already include this in their own charge.
const CANONICAL_INDEX_OVERHEAD_BYTES: u64 = 96;

/// Optional logical-charge aggregate for diagnostics. A read failure records
/// unknown rather than failing the migration.
fn logical_charge(conn: &Connection) -> Option<u64> {
    conn.query_row(
        "SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages",
        [],
        |row| row.get::<_, i64>(0),
    )
    .ok()
    .and_then(|charge| u64::try_from(charge).ok())
}

/// Validate every known consumer of a page before its body may be dropped.
/// Work receipts, work retries/scans and selection sources all retain the old
/// durable identity; a hash, scope, ordinal or confirmation mismatch pins the
/// page as raw rather than silently discarding the proof a consumer relies on.
fn dependencies_are_valid(conn: &Connection, page: &StoredPage) -> Result<bool, StateError> {
    let identities = page.page.identities.len();
    let invalid_ordinal: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_work_retries WHERE page_id=?1 AND (ordinal<0 OR ordinal>=?2)) OR EXISTS(SELECT 1 FROM provider_work_scan WHERE page_id=?1 AND (ordinal<0 OR ordinal>=?2))",
        params![page.id.0, i64::try_from(identities).map_err(|_error| StateError::ProviderInboxInvalid)?],
        |row| row.get(0),
    )?;
    if invalid_ordinal {
        return Ok(false);
    }
    let mut receipts = conn.prepare(
        "SELECT ordinal,confirmation_hash,confirmation,source_hash,scope FROM provider_work_receipts WHERE page_id=?1",
    )?;
    let mut rows = receipts.query([page.id.0])?;
    // Stream receipts so at most one confirmation blob is resident while each
    // stored confirmation is checked.
    while let Some(row) = rows.next()? {
        let ordinal: i64 = row.get(0)?;
        let confirmation_hash: String = row.get(1)?;
        let confirmation: Option<Vec<u8>> = row.get(2)?;
        let source_hash: String = row.get(3)?;
        let scope: String = row.get(4)?;
        let expected_confirmation = confirmation
            .as_ref()
            .map_or_else(String::new, |body| format!("{:x}", Sha256::digest(body)));
        if usize::try_from(ordinal)
            .ok()
            .is_none_or(|ordinal| ordinal >= identities)
            || confirmation_hash != expected_confirmation
            || source_hash != page.observation_hash
            || scope != page.page.scope
        {
            return Ok(false);
        }
    }
    let sources: Vec<(i64, String)> = conn
        .prepare("SELECT ordinal,body_hash FROM provider_selection_sources WHERE page_id=?1")?
        .query_map([page.id.0], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (ordinal, body_hash) in sources {
        if usize::try_from(ordinal)
            .ok()
            .is_none_or(|ordinal| ordinal >= identities)
            || body_hash != page.observation_hash
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Recompute every stored projection fact for a page and compare it with a
/// fresh pure projection of retained bytes. Unsupported projector versions,
/// unknown shapes, and any mismatch pin the page as raw.
fn projection_is_valid(conn: &Connection, page: &StoredPage) -> Result<bool, StateError> {
    use std::collections::BTreeSet;

    // Projection proof is optional evidence for dropping a duplicate body. A
    // page whose normalized reference paths exceed the projection budget stays
    // raw; only SQLite/storage failures while reading retained facts propagate.
    let Ok(plan) = crate::icloud::photos::plan_retained(page) else {
        return Ok(false);
    };
    let receipt: Option<(i64, String, i64, i64, i64, i64)> = conn.query_row(
        "SELECT projector_version,body_hash,record_count,reference_count,debt_count,charged_bytes FROM provider_catalog_pages WHERE page_id=?1",
        [page.id.0],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
    ).optional()?;
    let Some((version, body_hash, records, references, debt, charged)) = receipt else {
        // No projection exists, so no derived fact is being retained. A work
        // or selection reference without its source projection is ambiguous
        // and pins the original page.
        let dependent: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_work_receipts WHERE page_id=?1) OR EXISTS(SELECT 1 FROM provider_work_obligations WHERE page_id=?1) OR EXISTS(SELECT 1 FROM provider_selection_sources WHERE page_id=?1)",
            [page.id.0], |row| row.get(0),
        )?;
        return Ok(!dependent);
    };
    if version != crate::state::db::provider_catalog::PROJECTOR_VERSION
        || body_hash != page.observation_hash
        || usize::try_from(records).ok() != Some(plan.records.len())
        || usize::try_from(references).ok()
            != Some(plan.records.iter().map(|r| r.references.len()).sum())
        || usize::try_from(debt).ok() != Some(plan.records.iter().map(|r| r.debt.len()).sum())
        || u64::try_from(charged).ok() != Some(plan.charged_bytes)
    {
        return Ok(false);
    }
    let stored: Vec<(i64,String,Option<String>,i64,String)> = conn.prepare(
        "SELECT ordinal,record_name,record_type,deleted,kind FROM provider_catalog_records WHERE page_id=?1 ORDER BY ordinal",
    )?.query_map([page.id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?
        .collect::<Result<Vec<_>,_>>()?;
    if stored.len() != plan.records.len() {
        return Ok(false);
    }
    for (ordinal, (row, (identity, record))) in stored
        .iter()
        .zip(page.page.identities.iter().zip(&plan.records))
        .enumerate()
    {
        if row.0 != i64::try_from(ordinal).map_err(|_error| StateError::ProviderCatalogInvalid)?
            || row.1 != identity.name
            || row.2 != identity.record_type
            || row.3 != i64::from(identity.deleted)
            || row.4 != record.kind
        {
            return Ok(false);
        }
    }
    let expected_references: BTreeSet<_> = plan
        .records
        .iter()
        .enumerate()
        .flat_map(|(ordinal, record)| {
            record.references.iter().map(move |reference| {
                (
                    ordinal as i64,
                    reference.path.clone(),
                    reference.target.clone(),
                    reference.zone_name.clone(),
                    reference.zone_owner.clone(),
                )
            })
        })
        .collect();
    let actual_references: BTreeSet<_> = conn.prepare(
        "SELECT ordinal,field_path,target_record_name,target_zone_name,target_zone_owner FROM provider_catalog_references WHERE page_id=?1",
    )?.query_map([page.id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?
        .collect::<Result<_,_>>()?;
    let expected_debt: BTreeSet<_> = plan
        .records
        .iter()
        .enumerate()
        .flat_map(|(ordinal, record)| {
            record
                .debt
                .iter()
                .map(move |debt| (ordinal as i64, debt.path.clone(), debt.reason.to_owned()))
        })
        .collect();
    let actual_debt: BTreeSet<_> = conn
        .prepare("SELECT ordinal,field_path,reason FROM provider_catalog_debt WHERE page_id=?1")?
        .query_map([page.id.0], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(expected_references == actual_references
        && expected_debt == actual_debt
        && dependencies_are_valid(conn, page)?)
}

/// Find a retained raw page that the classifier proves equivalent to an
/// incoming page while no canonical index exists. Candidate rows are streamed
/// by the existing scoped lookup; the caller writes only after this returns.
fn find_legacy_equivalent(
    conn: &Connection,
    account_key: &str,
    provider_key: &str,
    page: &ObservedPage,
    evidence: &crate::icloud::photos::transport_evidence::TransportEvidence,
    canonical_sha: &str,
) -> Result<Option<CapturedPageId>, StateError> {
    let mut candidates = conn.prepare(
        "SELECT p.id FROM provider_shadow_pages p WHERE p.account_key=?1 AND p.provider_key=?2 AND p.scope=?3 AND p.request_cursor=?4 AND p.successor=?5 AND p.more_coming=?6 AND NOT EXISTS(SELECT 1 FROM provider_transport_canonical c WHERE c.page_id=p.id) AND NOT EXISTS(SELECT 1 FROM provider_transport_aliases a WHERE a.page_id=p.id) ORDER BY p.id",
    )?;
    let mut rows = candidates.query(params![
        account_key,
        provider_key,
        page.scope,
        page.request_cursor,
        page.successor,
        page.more_coming,
    ])?;
    while let Some(row) = rows.next()? {
        let id = CapturedPageId(row.get(0)?);
        let candidate = load_page(
            conn,
            id,
            crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
        )?;
        if !matches!(candidate.provenance, PageProvenance::Raw)
            || !same_capture_metadata(&candidate.page, page)
        {
            continue;
        }
        let Some(candidate_evidence) =
            crate::icloud::photos::transport_evidence::capture_evidence(&candidate.page)
                .map_err(|_error| StateError::ProviderInboxInvalid)?
        else {
            continue;
        };
        if candidate_evidence.version != evidence.version
            || candidate_evidence.masked != evidence.masked
            || canonical_key(
                &candidate.account_key,
                &candidate.provider_key,
                &candidate.page,
                &candidate_evidence,
            )? != canonical_sha
            || !projection_is_valid(conn, &candidate)?
            || !dependencies_are_valid(conn, &candidate)?
        {
            continue;
        }
        return Ok(Some(id));
    }
    Ok(None)
}

/// Replace strictly equivalent legacy transport duplicate bodies with typed
/// aliases. Runs inside the schema migration savepoint, streams one body at a
/// time, and never aliases a page whose projection, references, debt, owner,
/// raw hash, or classifier evidence cannot be proved.
pub(crate) fn compact_legacy_aliases(
    conn: &Connection,
) -> Result<crate::support::CompactionSummary, StateError> {
    use crate::support::CompactionSummary;
    use std::collections::HashMap;

    let mut summary = CompactionSummary::default();
    let before_charge = logical_charge(conn);
    let ids: Vec<i64> = conn.prepare(
        "SELECT p.id FROM provider_shadow_pages p LEFT JOIN provider_transport_aliases a ON a.page_id=p.id WHERE a.page_id IS NULL ORDER BY p.id",
    )?.query_map([], |row| row.get(0))?.collect::<Result<_,_>>()?;
    let mut groups: HashMap<String, (u8, Vec<i64>)> = HashMap::new();
    for id in ids {
        let page = load_page(
            conn,
            CapturedPageId(id),
            crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
        )?;
        if !matches!(page.provenance, PageProvenance::Raw) {
            summary.retained_unsupported_shape =
                summary.retained_unsupported_shape.saturating_add(1);
            continue;
        }
        summary.examined_raw_pages = summary.examined_raw_pages.saturating_add(1);
        if page.page.body.is_empty() {
            summary.retained_unsupported_shape =
                summary.retained_unsupported_shape.saturating_add(1);
            continue;
        }
        let Some(evidence) =
            crate::icloud::photos::transport_evidence::capture_evidence(&page.page)
                .map_err(|_error| StateError::ProviderInboxInvalid)?
        else {
            summary.retained_unsupported_shape =
                summary.retained_unsupported_shape.saturating_add(1);
            continue;
        };
        let key = canonical_key(&page.account_key, &page.provider_key, &page.page, &evidence)?;
        if !projection_is_valid(conn, &page)? {
            summary.retained_projection_or_dependency =
                summary.retained_projection_or_dependency.saturating_add(1);
            continue;
        }
        summary.eligible = summary.eligible.saturating_add(1);
        groups
            .entry(key)
            .or_insert_with(|| (evidence.version, Vec::new()))
            .1
            .push(id);
    }
    for (certificate, (version, members)) in groups {
        if members.len() < 2 {
            summary.no_equivalent_peer = summary.no_equivalent_peer.saturating_add(1);
            continue;
        }
        let group_pages = u64::try_from(members.len()).unwrap_or(u64::MAX);
        // An existing canonical index is authoritative. A representative that
        // failed validation must pin the whole group rather than being replaced.
        let existing: Option<i64> = conn.query_row(
            "SELECT page_id FROM provider_transport_canonical WHERE canonical_sha=?1 AND version=?2",
            params![certificate, i64::from(version)],
            |row| row.get(0),
        ).optional()?;
        let canonical_id = match existing {
            Some(id) if members.contains(&id) => id,
            Some(_) => {
                summary.invalid_canonical_or_member = summary
                    .invalid_canonical_or_member
                    .saturating_add(group_pages);
                continue;
            }
            None => *members
                .iter()
                .min()
                .ok_or(StateError::ProviderInboxInvalid)?,
        };
        let canonical = load_page(
            conn,
            CapturedPageId(canonical_id),
            crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
        )?;
        let canonical_evidence =
            crate::icloud::photos::transport_evidence::capture_evidence(&canonical.page)
                .map_err(|_error| StateError::ProviderInboxInvalid)?
                .ok_or(StateError::ProviderInboxInvalid)?;
        if canonical_evidence.version != version
            || canonical_key(
                &canonical.account_key,
                &canonical.provider_key,
                &canonical.page,
                &canonical_evidence,
            )? != certificate
        {
            summary.invalid_canonical_or_member = summary
                .invalid_canonical_or_member
                .saturating_add(group_pages);
            continue;
        }
        summary.retained_representative = summary.retained_representative.saturating_add(1);
        if existing.is_none() {
            conn.execute(
                "INSERT INTO provider_transport_canonical(canonical_sha,version,page_id) VALUES (?1,?2,?3)",
                params![certificate, i64::from(version), canonical_id],
            )?;
            conn.execute(
                "UPDATE provider_shadow_pages SET charged_bytes=charged_bytes+?2 WHERE id=?1",
                params![
                    canonical_id,
                    i64::try_from(CANONICAL_INDEX_OVERHEAD_BYTES)
                        .map_err(|_error| StateError::ProviderInboxInvalid)?
                ],
            )?;
        }
        for member_id in members {
            if member_id == canonical_id {
                continue;
            }
            let member = load_page(
                conn,
                CapturedPageId(member_id),
                crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
            )?;
            let member_evidence =
                crate::icloud::photos::transport_evidence::capture_evidence(&member.page)
                    .map_err(|_error| StateError::ProviderInboxInvalid)?
                    .ok_or(StateError::ProviderInboxInvalid)?;
            let masked_sha = format!("{:x}", Sha256::digest(&member_evidence.masked));
            if member_evidence.version != version
                || member_evidence.masked != canonical_evidence.masked
                || masked_sha != format!("{:x}", Sha256::digest(&canonical_evidence.masked))
                || !same_capture_metadata(&member.page, &canonical.page)
            {
                summary.invalid_canonical_or_member =
                    summary.invalid_canonical_or_member.saturating_add(1);
                continue;
            }
            if !projection_is_valid(conn, &member)? {
                summary.retained_projection_or_dependency =
                    summary.retained_projection_or_dependency.saturating_add(1);
                continue;
            }
            let body_len = member.page.body.len() as u64;
            let old_charge = conn.query_row(
                "SELECT charged_bytes FROM provider_shadow_pages WHERE id=?1",
                [member_id],
                |row| row.get::<_, i64>(0),
            )?;
            let old_charge =
                u64::try_from(old_charge).map_err(|_error| StateError::ProviderInboxInvalid)?;
            let new_charge = old_charge
                .saturating_sub(body_len)
                .saturating_add(ALIAS_OVERHEAD_BYTES);
            if new_charge >= old_charge {
                summary.no_positive_saving = summary.no_positive_saving.saturating_add(1);
                continue;
            }
            // Certificate fields repeat only this proven pair. The alias row
            // itself is the only added durable identity; replay of the old URL
            // variant returns the canonical page and creates no new alias.
            let provenance_hash = alias_provenance_hash(
                member_id,
                &member.observation_hash,
                canonical_id,
                &canonical.observation_hash,
                &certificate,
                version,
                &member.page.identities,
            )?;
            let inserted = conn.execute(
                "INSERT INTO provider_transport_aliases(page_id,canonical_page_id,classifier_version,masked_sha,certificate,provenance_hash) VALUES (?1,?2,?3,?4,?5,?6)",
                params![member_id, canonical_id, i64::from(version), masked_sha, certificate, provenance_hash],
            )?;
            if inserted != 1 {
                return Err(StateError::ProviderInboxInvalid);
            }
            conn.execute(
                "UPDATE provider_shadow_pages SET body=X'',charged_bytes=?2 WHERE id=?1",
                params![
                    member_id,
                    i64::try_from(new_charge).map_err(|_error| StateError::ProviderInboxInvalid)?
                ],
            )?;
            summary.compacted_aliases = summary.compacted_aliases.saturating_add(1);
            #[cfg(test)]
            {
                let remaining = TEST_ABORT_AFTER_ALIASES.with(std::cell::Cell::get);
                if remaining > 0 {
                    TEST_ABORT_AFTER_ALIASES.with(|count| count.set(remaining - 1));
                    if remaining == 1 {
                        return Err(StateError::ProviderInboxInvalid);
                    }
                }
            }
        }
    }
    summary.logical_charge_before = before_charge;
    summary.logical_charge_after = logical_charge(conn);
    Ok(summary)
}

#[cfg(test)]
thread_local! {
    /// Test-only deterministic interruption point after a durable alias write.
    pub(crate) static TEST_ABORT_AFTER_ALIASES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl SqliteStateDb {
    pub(crate) async fn capture_shadow_page(
        &self,
        owner: account::AccountOwner,
        page: ObservedPage,
        capacity: u64,
    ) -> Result<CapturedPageId, StateError> {
        self.with_conn_mut("capturing provider shadow page", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate(&tx, &owner)?;
            let (account_key, provider_key): (String, String) = tx.query_row(
                "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
                [], |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let hash = format!("{:x}", Sha256::digest(&page.body));
            // The provider owns this recomputation. Storage never trusts an
            // adapter-supplied classification and never parses provider bytes.
            let evidence = crate::icloud::photos::transport_evidence::capture_evidence(&page)
                .map_err(|_error| StateError::ProviderInboxInvalid)?;
            let canonical = evidence
                .as_ref()
                .map(|evidence| {
                    canonical_key(&account_key, &provider_key, &page, evidence)
                        .map(|sha| (sha, evidence.version))
                })
                .transpose()?;
            let existing: Option<i64> = tx.query_row(
                "SELECT id FROM provider_shadow_pages WHERE scope=?1 AND request_cursor=?2 AND body_hash=?3",
                params![page.scope, page.request_cursor, hash],
                |row| row.get(0),
            ).optional()?;
            let storage: (StorageOutcome, Option<(u64, u64)>);
            let page_id = if let Some(id) = existing {
                let original = load_page(
                    &tx,
                    CapturedPageId(id),
                    crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
                )?;
                match &original.provenance {
                    PageProvenance::Raw => {
                        if original.page.body != page.body
                            || !same_capture_metadata(&original.page, &page)
                        {
                            return Err(StateError::ProviderInboxInvalid);
                        }
                        storage = (StorageOutcome::Exact, None);
                        id
                    }
                    PageProvenance::LegacyAlias(alias) => {
                        // The request bytes are the discarded historical
                        // variant, so they can never equal the representative.
                        // Prove the typed alias instead: same complete masked
                        // bytes, classifier, and capture metadata. Return the
                        // canonical ID so replay never creates a new alias.
                        let incoming = evidence
                            .as_ref()
                            .ok_or(StateError::ProviderInboxInvalid)?;
                        let masked_sha = format!("{:x}", Sha256::digest(&incoming.masked));
                        if masked_sha != alias.masked_sha
                            || incoming.version != alias.classifier_version
                            || !same_capture_metadata(&original.page, &page)
                        {
                            return Err(StateError::ProviderInboxInvalid);
                        }
                        storage = (StorageOutcome::Equivalent, None);
                        alias.canonical_page_id.0
                    }
                }
            } else {
                let canonical_hit = match canonical.as_ref() {
                    Some((canonical_sha, _)) => tx
                        .query_row(
                            "SELECT page_id,version FROM provider_transport_canonical WHERE canonical_sha=?1",
                            [canonical_sha],
                            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                        )
                        .optional()?,
                    None => None,
                };
                let mut legacy_candidate = false;
                let canonical_hit = if canonical_hit.is_some() {
                    canonical_hit
                } else if let Some((canonical_sha, version)) = canonical.as_ref() {
                    let incoming = evidence.as_ref().ok_or(StateError::ProviderInboxInvalid)?;
                    let found = find_legacy_equivalent(
                        &tx,
                        &account_key,
                        &provider_key,
                        &page,
                        incoming,
                        canonical_sha,
                    )?;
                    legacy_candidate = found.is_some();
                    found.map(|id| (id.0, i64::from(*version)))
                } else {
                    None
                };
                if let Some((representative_id, stored_version)) = canonical_hit {
                    let (canonical_sha, version) =
                        canonical.as_ref().ok_or(StateError::ProviderInboxInvalid)?;
                    let representative = load_page(
                        &tx,
                        CapturedPageId(representative_id),
                        crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
                    )?;
                    if !matches!(representative.provenance, PageProvenance::Raw) {
                        return Err(StateError::ProviderInboxInvalid);
                    }
                    let representative_evidence =
                        crate::icloud::photos::transport_evidence::capture_evidence(
                            &representative.page,
                        )
                        .map_err(|_error| StateError::ProviderInboxInvalid)?
                        .ok_or(StateError::ProviderInboxInvalid)?;
                    let recomputed = canonical_key(
                        &representative.account_key,
                        &representative.provider_key,
                        &representative.page,
                        &representative_evidence,
                    )?;
                    let incoming = evidence.as_ref().ok_or(StateError::ProviderInboxInvalid)?;
                    if recomputed != *canonical_sha
                        || u8::try_from(stored_version).ok() != Some(*version)
                        || representative_evidence.version != incoming.version
                        || representative_evidence.masked != incoming.masked
                        || !same_capture_metadata(&representative.page, &page)
                    {
                        return Err(StateError::ProviderInboxInvalid);
                    }
                    if legacy_candidate {
                        let used: i64 = tx.query_row(
                            "SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages",
                            [],
                            |row| row.get(0),
                        )?;
                        let used = u64::try_from(used)
                            .map_err(|_error| StateError::ProviderInboxInvalid)?;
                        if used.saturating_add(CANONICAL_INDEX_OVERHEAD_BYTES) <= capacity {
                            let inserted = tx.execute(
                                "INSERT INTO provider_transport_canonical(canonical_sha,version,page_id) VALUES (?1,?2,?3) ON CONFLICT(canonical_sha,version) DO NOTHING",
                                params![canonical_sha, i64::from(*version), representative_id],
                            )?;
                            if inserted == 1 {
                                tx.execute(
                                    "UPDATE provider_shadow_pages SET charged_bytes=charged_bytes+?2 WHERE id=?1",
                                    params![
                                        representative_id,
                                        i64::try_from(CANONICAL_INDEX_OVERHEAD_BYTES)
                                            .map_err(|_error| StateError::ProviderInboxInvalid)?
                                    ],
                                )?;
                            }
                        }
                    }
                    storage = (StorageOutcome::Equivalent, None);
                    representative_id
                } else {
                    // Charge UTF-8 payload and provenance bytes, including source
                    // identities. This is an inbox budget, not a disk-file quota.
                    let charge = page.body.len() as u64 + page.scope.len() as u64
                        + page.request_cursor.len() as u64 + page.successor.len() as u64
                        + account_key.len() as u64 + provider_key.len() as u64 + 128
                        + page.identities.iter().map(|id| id.name.len() as u64
                            + id.record_type.as_ref().map_or(0, |kind| kind.len() as u64) + 32).sum::<u64>()
                        + canonical.as_ref().map_or(0, |_| 64 + 32);
                    let used: i64 = tx.query_row(
                        "SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages", [], |row| row.get(0),
                    )?;
                    let used = u64::try_from(used).map_err(|_error| StateError::ProviderInboxInvalid)?;
                    if charge > capacity.saturating_sub(used) {
                        crate::support::record_storage(
                            StorageComponent::Inbox,
                            StorageOutcome::CapacityRejected,
                            Some((used, capacity)),
                        );
                        return Err(StateError::ProviderInboxFull);
                    }
                    storage = (StorageOutcome::New, Some((used, capacity)));
                    let charge = i64::try_from(charge).map_err(|_error| StateError::ProviderInboxFull)?;
                    tx.execute(
                        "INSERT INTO provider_shadow_pages(account_key,provider_key,scope,request_cursor,successor,more_coming,body_hash,body,charged_bytes,observed_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                        params![account_key, provider_key, page.scope, page.request_cursor,
                            page.successor, page.more_coming, hash, page.body, charge, chrono::Utc::now().timestamp()],
                    )?;
                    let id = tx.last_insert_rowid();
                    if let Some((canonical_sha, version)) = canonical.as_ref() {
                        tx.execute(
                            "INSERT INTO provider_transport_canonical(canonical_sha,version,page_id) VALUES (?1,?2,?3) ON CONFLICT(canonical_sha,version) DO NOTHING",
                            params![canonical_sha, i64::from(*version), id],
                        )?;
                    }
                    for (ordinal, identity) in page.identities.iter().enumerate() {
                        let ordinal = i64::try_from(ordinal).map_err(|_error| StateError::ProviderInboxInvalid)?;
                        tx.execute(
                            "INSERT INTO provider_shadow_records(page_id,ordinal,record_name,record_type,deleted) VALUES (?1,?2,?3,?4,?5)",
                            params![id, ordinal, identity.name, identity.record_type, identity.deleted],
                        )?;
                    }
                    id
                }
            };
            // Last observed receipt only. Replaying an older legacy cursor may
            // replace this pointer; it is neither materialization nor coverage.
            tx.execute(
                "INSERT INTO provider_shadow_receipts(scope,page_id) VALUES (?1,?2) ON CONFLICT(scope) DO UPDATE SET page_id=excluded.page_id",
                params![page.scope, page_id],
            )?;
            tx.commit()?;
            crate::support::record_storage(StorageComponent::Inbox, storage.0, storage.1);
            Ok(CapturedPageId(page_id))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CapturedPageId, ObservedPage, PageProvenance, SourceIdentity, StoredPage,
        TEST_ABORT_AFTER_ALIASES, load_page,
    };
    use crate::state::db::account::AccountOwner;
    use rusqlite::{Connection, params};
    use sha2::{Digest, Sha256};

    const USER: &str = "legacy-compaction@example.invalid";

    fn owner_record() -> AccountOwner {
        let data = serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"legacy-provider"}}))
            .unwrap();
        AccountOwner::authenticated(USER, "com", &data).unwrap()
    }

    fn scope() -> String {
        serde_json::json!({"format":1,"realm":"legacy-realm","container":"com.apple.photos.cloud",
            "environment":"production","database":"private",
            "zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}})
        .to_string()
    }

    fn body(url: &str) -> Vec<u8> {
        format!(r#"{{"zones":[{{"zoneID":{{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},"syncToken":"successor","moreComing":false,"records":[{{"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":"tag-1","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":1000,"downloadURL":"{url}","fileChecksum":"checksum-A","referenceChecksum":"reference-A","wrappingKey":"wrapping-A"}}}}}}}}]}}]}}"#).into_bytes()
    }

    fn observed(url: &str) -> ObservedPage {
        ObservedPage {
            scope: scope(),
            request_cursor: "cursor-1".to_owned(),
            successor: "successor".to_owned(),
            more_coming: false,
            body: body(url),
            identities: vec![SourceIdentity {
                name: "master-1".to_owned(),
                record_type: Some("CPLMaster".to_owned()),
                deleted: false,
            }],
        }
    }

    fn insert_legacy(conn: &Connection, page: &ObservedPage, charge: i64) -> i64 {
        let hash = format!("{:x}", Sha256::digest(&page.body));
        conn.execute(
            "INSERT INTO provider_shadow_pages(account_key,provider_key,scope,request_cursor,successor,more_coming,body_hash,body,charged_bytes,observed_at) VALUES ((SELECT account_key FROM account_owner),(SELECT provider_key FROM account_owner),?1,?2,?3,?4,?5,?6,?7,1)",
            params![page.scope,page.request_cursor,page.successor,page.more_coming,hash,page.body,charge],
        ).unwrap();
        let id = conn.last_insert_rowid();
        for (ordinal, identity) in page.identities.iter().enumerate() {
            conn.execute(
                "INSERT INTO provider_shadow_records(page_id,ordinal,record_name,record_type,deleted) VALUES (?1,?2,?3,?4,?5)",
                params![id,ordinal as i64,identity.name,identity.record_type,identity.deleted],
            ).unwrap();
        }
        id
    }

    fn insert_projection(conn: &Connection, id: i64, page: &ObservedPage) {
        let (account_key, provider_key): (String, String) = conn
            .query_row(
                "SELECT account_key,provider_key FROM account_owner",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let source = StoredPage {
            id: CapturedPageId(id),
            account_key,
            provider_key,
            observation_hash: format!("{:x}", Sha256::digest(&page.body)),
            provenance: PageProvenance::Raw,
            page: page.clone(),
        };
        let plan = crate::icloud::photos::plan_retained(&source).unwrap();
        for (ordinal, (record, identity)) in plan.records.iter().zip(&page.identities).enumerate() {
            let ordinal = ordinal as i64;
            conn.execute(
                "INSERT INTO provider_catalog_records(page_id,ordinal,record_name,record_type,deleted,kind) VALUES (?1,?2,?3,?4,?5,?6)",
                params![id,ordinal,identity.name,identity.record_type,identity.deleted,record.kind],
            ).unwrap();
            for reference in &record.references {
                conn.execute(
                    "INSERT INTO provider_catalog_references(page_id,ordinal,field_path,target_record_name,target_zone_name,target_zone_owner) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![id,ordinal,reference.path,reference.target,reference.zone_name,reference.zone_owner],
                ).unwrap();
            }
            for debt in &record.debt {
                conn.execute(
                    "INSERT INTO provider_catalog_debt(page_id,ordinal,field_path,reason) VALUES (?1,?2,?3,?4)",
                    params![id,ordinal,debt.path,debt.reason],
                ).unwrap();
            }
        }
        conn.execute(
            "INSERT INTO provider_catalog_pages(page_id,projector_version,body_hash,record_count,reference_count,debt_count,charged_bytes) VALUES (?1,1,?2,?3,?4,?5,?6)",
            params![id,source.observation_hash,plan.records.len() as i64,
                plan.records.iter().map(|r|r.references.len()).sum::<usize>() as i64,
                plan.records.iter().map(|r|r.debt.len()).sum::<usize>() as i64,
                plan.charged_bytes as i64],
        ).unwrap();
    }

    fn legacy_v35(path: &std::path::Path) -> (AccountOwner, ObservedPage, ObservedPage) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE account_owner(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,account_key TEXT NOT NULL,provider_key TEXT NOT NULL);").unwrap();
        conn.execute(
            "INSERT INTO account_owner VALUES(1,1,?1,?2)",
            params![
                crate::account::namespace(USER, "com"),
                crate::account::provider_fingerprint("com", "legacy-provider")
            ],
        )
        .unwrap();
        for version in 1..=35 {
            crate::state::schema::migrate_to_version(&conn, 0, version).unwrap();
        }
        let first = observed("https://p01.icloud-content.com/asset-1?token=first");
        let second = observed("https://p99.icloud-content.com/another/path?token=second");
        insert_legacy(&conn, &first, 5000);
        insert_legacy(&conn, &second, 5000);
        insert_projection(&conn, 1, &first);
        insert_projection(&conn, 2, &second);
        conn.execute("INSERT INTO metadata(key,value) VALUES ('sync_token:PrimarySync','saved'),('pending_sync_token:old:PrimarySync','debt')", []).unwrap();
        (owner_record(), first, second)
    }

    fn charged_sum(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT SUM(charged_bytes) FROM provider_shadow_pages",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn fixture_body(group: usize, url: &str) -> Vec<u8> {
        let padding = "p".repeat(1400);
        format!(
            r#"{{"zones":[{{"zoneID":{{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},"syncToken":"successor","moreComing":false,"records":[{{"recordName":"master-{group}","recordType":"CPLMaster","recordChangeTag":"tag-{group}","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":1000,"downloadURL":"{url}","fileChecksum":"checksum-{group}","referenceChecksum":"reference-{group}","wrappingKey":"wrapping-{group}"}}}},"brokenRef":{{"type":"REFERENCE","value":{{}}}},"padding":{{"value":"{padding}"}}}}}}]}}]}}"#
        )
        .into_bytes()
    }

    fn fixture_observed(group: usize, url: &str) -> ObservedPage {
        ObservedPage {
            scope: scope(),
            request_cursor: "cursor-1".to_owned(),
            successor: "successor".to_owned(),
            more_coming: false,
            body: fixture_body(group, url),
            identities: vec![SourceIdentity {
                name: format!("master-{group}"),
                record_type: Some("CPLMaster".to_owned()),
                deleted: false,
            }],
        }
    }

    /// Exact released admission charge. Legacy schema 35 rows predate the
    /// canonical index, so the optional canonical overhead is not included.
    fn released_charge(page: &ObservedPage, account_key: &str, provider_key: &str) -> u64 {
        page.body.len() as u64
            + page.scope.len() as u64
            + page.request_cursor.len() as u64
            + page.successor.len() as u64
            + account_key.len() as u64
            + provider_key.len() as u64
            + 128
            + page
                .identities
                .iter()
                .map(|identity| {
                    identity.name.len() as u64
                        + identity
                            .record_type
                            .as_ref()
                            .map_or(0, |kind| kind.len() as u64)
                        + 32
                })
                .sum::<u64>()
    }

    /// A genuine schema 35 inbox with 35 released-charge rows, five transport
    /// classes of seven URL-only duplicates, plus a checkpoint, real catalogue
    /// debt, and work/selection references to the first alias candidate.
    fn full_legacy_v35(path: &std::path::Path) -> (AccountOwner, u64) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE account_owner(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,account_key TEXT NOT NULL,provider_key TEXT NOT NULL);").unwrap();
        conn.execute(
            "INSERT INTO account_owner VALUES(1,1,?1,?2)",
            params![
                crate::account::namespace(USER, "com"),
                crate::account::provider_fingerprint("com", "legacy-provider")
            ],
        )
        .unwrap();
        for version in 1..=35 {
            crate::state::schema::migrate_to_version(&conn, 0, version).unwrap();
        }
        let (account_key, provider_key): (String, String) = conn
            .query_row(
                "SELECT account_key,provider_key FROM account_owner",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let mut budget = 0u64;
        let mut reference_id = None;
        for group in 0..5 {
            for member in 0..7 {
                let url = format!(
                    "https://p{:02}.icloud-content.com/asset-{group}?token={member}",
                    member + 1
                );
                let page = fixture_observed(group, &url);
                let charge = released_charge(&page, &account_key, &provider_key);
                let id = insert_legacy(&conn, &page, i64::try_from(charge).unwrap());
                insert_projection(&conn, id, &page);
                budget += charge;
                if group == 0 && member == 1 {
                    reference_id = Some((id, format!("{:x}", Sha256::digest(&page.body))));
                }
            }
        }
        let (reference_id, reference_hash) = reference_id.unwrap();
        let confirmation = b"independent confirmation bytes".to_vec();
        let confirmation_hash = format!("{:x}", Sha256::digest(&confirmation));
        conn.execute(
            "INSERT INTO provider_work_receipts(page_id,ordinal,config_hash,confirmation_hash,confirmation,state,reason,scope,source_hash,master_record_name,charged_bytes) VALUES (?1,0,?2,?3,?4,'admitted','',?5,?6,NULL,1)",
            params![reference_id, "a".repeat(64), confirmation_hash, confirmation, scope(), reference_hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_selection_generations(id,account_key,provider_key,format,scope,config_hash,manifest,charged_bytes) VALUES ('generation-1',?1,?2,1,?3,?4,X'00',1)",
            params![account_key, provider_key, scope(), "b".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_selection_sources(generation,page_id,ordinal,body_hash) VALUES ('generation-1',?1,0,?2)",
            params![reference_id, reference_hash],
        )
        .unwrap();
        conn.execute("INSERT INTO metadata(key,value) VALUES ('sync_token:PrimarySync','saved'),('pending_sync_token:old:PrimarySync','debt')", []).unwrap();
        (owner_record(), budget)
    }

    #[tokio::test]
    async fn legacy_duplicates_compact_to_typed_aliases_then_capture_reuses_canonical() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.db");
        let (owner, first, second) = legacy_v35(&path);
        let (before, alias_hash) = {
            let conn = Connection::open(&path).unwrap();
            (
                charged_sum(&conn),
                format!("{:x}", Sha256::digest(&second.body)),
            )
        };
        {
            let conn = Connection::open(&path).unwrap();
            crate::state::schema::migrate(&conn).unwrap();
            assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_aliases",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
            assert!(charged_sum(&conn) < before);
            let alias_id: i64 = conn
                .query_row("SELECT page_id FROM provider_transport_aliases", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let canonical_id: i64 = conn
                .query_row(
                    "SELECT canonical_page_id FROM provider_transport_aliases",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(alias_id, 2);
            assert_eq!(canonical_id, 1);
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT length(body) FROM provider_shadow_pages WHERE id=?1",
                    [alias_id],
                    |r| r.get(0)
                )
                .unwrap(),
                0
            );
            assert_eq!(
                conn.query_row::<String, _, _>(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                "saved"
            );
            assert_eq!(
                conn.query_row::<String, _, _>(
                    "SELECT value FROM metadata WHERE key='pending_sync_token:old:PrimarySync'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                "debt"
            );
            let stored = load_page(&conn, CapturedPageId(alias_id), 16 * 1024 * 1024).unwrap();
            assert_eq!(stored.observation_hash, alias_hash);
            assert!(matches!(stored.provenance, PageProvenance::LegacyAlias(_)));
            assert_eq!(stored.page.body, first.body);
        }
        let db = crate::state::SqliteStateDb::open_owned(&path, &owner)
            .await
            .unwrap();
        let id = db
            .capture_shadow_page(owner.clone(), second.clone(), 512 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(id.0, 1);
        {
            let conn = db.acquire_lock("legacy alias replay").unwrap();
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_aliases",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
        }
        let mut novel = second.clone();
        novel.body = String::from_utf8(novel.body)
            .unwrap()
            .replace("master-1", "master-2")
            .replace("tag-1", "tag-2")
            .into_bytes();
        novel.identities[0].name = "master-2".to_owned();
        let novel_id = db
            .capture_shadow_page(owner.clone(), novel, 512 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(novel_id.0, 3);
        let conn = db.acquire_lock("legacy alias replay").unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_transport_aliases",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );

        // A corrupt representative or certificate refuses instead of yielding
        // fabricated original bytes or hashes.
        conn.execute("UPDATE provider_shadow_pages SET body=X'00' WHERE id=1", [])
            .unwrap();
        assert!(load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).is_err());
    }

    /// Schema 35 fixture with one released-charge page whose projection is
    /// valid but not canonically indexed; lazy capture must recover it.
    fn legacy_singleton_v35(path: &std::path::Path) -> (AccountOwner, ObservedPage) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE account_owner(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,account_key TEXT NOT NULL,provider_key TEXT NOT NULL);").unwrap();
        conn.execute(
            "INSERT INTO account_owner VALUES(1,1,?1,?2)",
            params![
                crate::account::namespace(USER, "com"),
                crate::account::provider_fingerprint("com", "legacy-provider")
            ],
        )
        .unwrap();
        for version in 1..=35 {
            crate::state::schema::migrate_to_version(&conn, 0, version).unwrap();
        }
        let (account_key, provider_key): (String, String) = conn
            .query_row(
                "SELECT account_key,provider_key FROM account_owner",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let page = observed("https://p01.icloud-content.com/asset-1?token=first");
        let charge = released_charge(&page, &account_key, &provider_key);
        let id = insert_legacy(&conn, &page, i64::try_from(charge).unwrap());
        insert_projection(&conn, id, &page);
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES ('sync_token:PrimarySync','saved'),('pending_sync_token:old:PrimarySync','debt')",
            [],
        )
        .unwrap();
        (owner_record(), page)
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Snapshot {
        pages: i64,
        canonical: i64,
        aliases: i64,
        charge: i64,
        body: Vec<u8>,
        hash: String,
        identities: Vec<(String, Option<String>, bool)>,
        catalog: (String, i64, i64, i64),
        checkpoints: (String, String),
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn snapshot(conn: &Connection) -> Snapshot {
        let (body, hash, charge) = conn
            .query_row(
                "SELECT body,body_hash,charged_bytes FROM provider_shadow_pages WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let mut stmt = conn.prepare("SELECT record_name,record_type,deleted FROM provider_shadow_records WHERE page_id=1 ORDER BY ordinal").unwrap();
        let identities = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let catalog = conn.query_row("SELECT body_hash,record_count,reference_count,debt_count FROM provider_catalog_pages WHERE page_id=1", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap();
        let checkpoints = conn.query_row("SELECT (SELECT value FROM metadata WHERE key='sync_token:PrimarySync'),(SELECT value FROM metadata WHERE key='pending_sync_token:old:PrimarySync')", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        Snapshot {
            pages: count(conn, "provider_shadow_pages"),
            canonical: count(conn, "provider_transport_canonical"),
            aliases: count(conn, "provider_transport_aliases"),
            charge,
            body,
            hash,
            identities,
            catalog,
            checkpoints,
        }
    }

    fn tight(before: i64) -> u64 {
        u64::try_from(before).unwrap() + super::CANONICAL_INDEX_OVERHEAD_BYTES
    }

    fn with_lazy_index(mut base: Snapshot) -> Snapshot {
        base.canonical = 1;
        base.charge += i64::try_from(super::CANONICAL_INDEX_OVERHEAD_BYTES).unwrap();
        base
    }

    fn evidence_identity(page: &ObservedPage) -> String {
        let evidence = crate::icloud::photos::transport_evidence::capture_evidence(page)
            .ok()
            .flatten()
            .expect("synthetic page must classify");
        format!("{:?}|{:?}", evidence.version, evidence.masked)
    }

    fn replace_body(mut page: ObservedPage, from: &str, to: &str) -> ObservedPage {
        page.body = String::from_utf8(page.body)
            .unwrap()
            .replace(from, to)
            .into_bytes();
        page
    }

    fn check(db: &crate::state::SqliteStateDb, label: &str, expected: &Snapshot) {
        let conn = db.acquire_lock(label).unwrap();
        assert_eq!(&snapshot(&conn), expected);
    }

    async fn open_ready(
        path: &std::path::Path,
        owner: &AccountOwner,
    ) -> (crate::state::SqliteStateDb, Snapshot) {
        let db = crate::state::SqliteStateDb::open_owned(path, owner)
            .await
            .unwrap();
        let baseline = {
            let conn = db.acquire_lock("open legacy fixture").unwrap();
            assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
            assert_eq!(count(&conn, "provider_transport_canonical"), 0);
            assert_eq!(count(&conn, "provider_transport_aliases"), 0);
            snapshot(&conn)
        };
        (db, baseline)
    }

    #[tokio::test]
    async fn legacy_singleton_url_variants_recover_once() {
        for pre_migrated in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory
                .path()
                .join(format!("singleton-{pre_migrated}.db"));
            let (owner, original) = legacy_singleton_v35(&path);
            if pre_migrated {
                let conn = Connection::open(&path).unwrap();
                crate::state::schema::migrate(&conn).unwrap();
            }
            let (db, baseline) = open_ready(&path, &owner).await;
            let replay = db
                .capture_shadow_page(owner.clone(), original.clone(), 512 * 1024 * 1024)
                .await
                .unwrap();
            assert_eq!(replay.0, 1);
            check(&db, "exact replay", &baseline);
            let original_evidence = evidence_identity(&original);
            for index in 0..65 {
                let variant = observed(&format!(
                    "https://p01.icloud-content.com/asset-{index}?token=variant-{index}"
                ));
                assert_eq!(evidence_identity(&variant), original_evidence);
                let id = db
                    .capture_shadow_page(owner.clone(), variant, tight(baseline.charge))
                    .await
                    .unwrap();
                assert_eq!(id.0, 1);
            }
            let expected = with_lazy_index(baseline.clone());
            {
                let conn = db.acquire_lock("indexed singleton").unwrap();
                assert_eq!(snapshot(&conn), expected);
                assert_eq!(
                    conn.query_row::<i64, _, _>(
                        "SELECT page_id FROM provider_transport_canonical",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                    1
                );
            }
            drop(db);
            let reopened = crate::state::SqliteStateDb::open_owned(&path, &owner)
                .await
                .unwrap();
            check(&reopened, "reopened singleton", &expected);
        }
    }

    #[tokio::test]
    async fn legacy_singleton_exact_capacity_defers_index_until_headroom() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("singleton-exact.db");
        let (owner, original) = legacy_singleton_v35(&path);
        let (db, baseline) = open_ready(&path, &owner).await;
        let exact = u64::try_from(baseline.charge).unwrap();
        let original_evidence = evidence_identity(&original);
        for index in 0..65 {
            let variant = observed(&format!(
                "https://p02.icloud-content.com/exact-{index}?token=exact-{index}"
            ));
            assert_eq!(evidence_identity(&variant), original_evidence);
            let id = db
                .capture_shadow_page(owner.clone(), variant, exact)
                .await
                .unwrap();
            assert_eq!(id.0, 1);
        }
        check(&db, "exact-capacity singleton", &baseline);
        drop(db);
        let reopened = crate::state::SqliteStateDb::open_owned(&path, &owner)
            .await
            .unwrap();
        check(&reopened, "reopened exact-capacity singleton", &baseline);
        assert_eq!(
            reopened
                .capture_shadow_page(
                    owner.clone(),
                    observed("https://p02.icloud-content.com/reopened?token=fresh"),
                    exact
                )
                .await
                .unwrap()
                .0,
            1
        );
        check(&reopened, "reopened no-index replay", &baseline);
        let expected = with_lazy_index(baseline.clone());
        let first = observed("https://p03.icloud-content.com/headroom?token=one");
        assert_eq!(evidence_identity(&first), original_evidence);
        let id = reopened
            .capture_shadow_page(owner.clone(), first, tight(baseline.charge))
            .await
            .unwrap();
        assert_eq!(id.0, 1);
        check(&reopened, "headroom indexed singleton", &expected);
        let second = observed("https://p04.icloud-content.com/again?token=two");
        let id = reopened
            .capture_shadow_page(owner.clone(), second, tight(baseline.charge))
            .await
            .unwrap();
        assert_eq!(id.0, 1);
        check(&reopened, "index charged once", &expected);
    }

    #[tokio::test]
    async fn legacy_singleton_rejects_non_equivalent_and_invalid_projection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("singleton-negative.db");
        let (owner, original) = legacy_singleton_v35(&path);
        let (db, baseline) = open_ready(&path, &owner).await;
        let capacity = tight(baseline.charge);
        let tag = replace_body(original.clone(), "\"tag-1\"", "\"tag-2\"");
        let unknown = replace_body(
            original.clone(),
            "\"recordChangeTag\":\"tag-1\"",
            "\"recordChangeTag\":\"tag-1\",\"unknownField\":{\"type\":\"UNKNOWN\",\"value\":\"surprise\"}",
        );
        let mut cursor = original.clone();
        cursor.request_cursor = "cursor-2".to_owned();
        let mut realm = original.clone();
        realm.scope = original
            .scope
            .replace("\"legacy-realm\"", "\"another-realm\"");
        assert!(serde_json::from_str::<serde_json::Value>(&realm.scope).is_ok());
        let shape = replace_body(
            original.clone(),
            "\"type\":\"ASSETID\"",
            "\"type\":\"FUTURE_ASSET\"",
        );
        for variant in [tag, unknown, cursor, realm, shape] {
            let error = db
                .capture_shadow_page(owner.clone(), variant, capacity)
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                crate::state::error::StateError::ProviderInboxFull
            ));
            check(&db, "non-equivalent refusal", &baseline);
        }
        {
            let conn = db.acquire_lock("corrupt projection").unwrap();
            conn.execute(
                "UPDATE provider_catalog_pages SET body_hash='corrupt' WHERE page_id=1",
                [],
            )
            .unwrap();
        }
        let corrupt = {
            let conn = db.acquire_lock("corrupt projection baseline").unwrap();
            snapshot(&conn)
        };
        let variant = observed("https://p05.icloud-content.com/projection?token=invalid");
        assert_eq!(evidence_identity(&variant), evidence_identity(&original));
        let error = db
            .capture_shadow_page(owner.clone(), variant, capacity)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::state::error::StateError::ProviderInboxFull
        ));
        check(&db, "invalid projection refusal", &corrupt);
    }

    #[tokio::test]
    async fn legacy_singleton_index_rolls_back_on_charge_trigger() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("singleton-trigger.db");
        let (owner, original) = legacy_singleton_v35(&path);
        let (db, baseline) = open_ready(&path, &owner).await;
        let capacity = tight(baseline.charge);
        let variant = observed("https://p06.icloud-content.com/trigger?token=blocked");
        assert_eq!(evidence_identity(&variant), evidence_identity(&original));
        {
            let conn = db.acquire_lock("install charge trigger").unwrap();
            conn.execute_batch(
                "CREATE TRIGGER fail_lazy_singleton_charge BEFORE UPDATE OF charged_bytes ON provider_shadow_pages BEGIN SELECT RAISE(ABORT, 'blocked lazy charge'); END;",
            )
            .unwrap();
        }
        assert!(
            db.capture_shadow_page(owner.clone(), variant.clone(), capacity)
                .await
                .is_err()
        );
        check(&db, "trigger rollback", &baseline);
        {
            let conn = db.acquire_lock("drop charge trigger").unwrap();
            conn.execute_batch("DROP TRIGGER fail_lazy_singleton_charge")
                .unwrap();
        }
        let id = db
            .capture_shadow_page(owner.clone(), variant, capacity)
            .await
            .unwrap();
        assert_eq!(id.0, 1);
        check(&db, "trigger retry", &with_lazy_index(baseline));
    }
    #[tokio::test]
    async fn legacy_singleton_damaged_evidence_stays_untouched() {
        for damage in ["hash", "unprojected_dependency"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("damaged.db");
            let (owner, original) = legacy_singleton_v35(&path);
            let (db, baseline) = open_ready(&path, &owner).await;
            {
                let conn = db.acquire_lock("damage singleton fixture").unwrap();
                if damage == "hash" {
                    conn.execute(
                        "UPDATE provider_shadow_pages SET body_hash='invalid' WHERE id=1",
                        [],
                    )
                    .unwrap();
                } else {
                    conn.execute("DELETE FROM provider_catalog_pages WHERE page_id=1", [])
                        .unwrap();
                    conn.execute("INSERT INTO provider_work_scan(scope,config_hash,page_id,ordinal) VALUES (?1,'config',1,99)", [&original.scope]).unwrap();
                }
            }
            let error = db
                .capture_shadow_page(
                    owner.clone(),
                    observed("https://p07.icloud-content.com/new?token=fresh"),
                    tight(baseline.charge),
                )
                .await
                .unwrap_err();
            if damage == "hash" {
                assert!(matches!(
                    error,
                    crate::state::error::StateError::ProviderCatalogInvalid
                ));
            } else {
                assert!(matches!(
                    error,
                    crate::state::error::StateError::ProviderInboxFull
                ));
            }
            let conn = db.acquire_lock("damaged evidence preserved").unwrap();
            assert_eq!(count(&conn, "provider_shadow_pages"), 1);
            assert_eq!(count(&conn, "provider_transport_canonical"), 0);
            assert_eq!(charged_sum(&conn), baseline.charge);
            let body: Vec<u8> = conn
                .query_row(
                    "SELECT body FROM provider_shadow_pages WHERE id=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(body, original.body);
            if damage == "unprojected_dependency" {
                assert_eq!(
                    conn.query_row::<i64, _, _>(
                        "SELECT ordinal FROM provider_work_scan",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap(),
                    99
                );
            }
        }
    }

    #[tokio::test]
    async fn legacy_singleton_normal_capture_reprojects_without_growth() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.db");
        let (owner, _) = legacy_singleton_v35(&path);
        let (db, baseline) = open_ready(&path, &owner).await;
        let db = std::sync::Arc::new(db);
        let mut capture = crate::icloud::photos::inbox::ShadowCapture::new(
            std::sync::Arc::clone(&db),
            owner,
            "legacy-realm",
        );
        capture.capacity = u64::try_from(baseline.charge).unwrap();
        for index in 0..3 {
            capture
                .capture(observed(&format!(
                    "https://p08.icloud-content.com/capture?token={index}"
                )))
                .await
                .unwrap();
        }
        check(&db, "normal capture unchanged", &baseline);
        let conn = db.acquire_lock("normal capture receipt").unwrap();
        assert_eq!(count(&conn, "provider_shadow_receipts"), 1);
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT page_id FROM provider_shadow_receipts", [], |r| r
                .get(0))
                .unwrap(),
            1
        );
    }

    fn oversized_unprojected_observed() -> ObservedPage {
        let targets: Vec<serde_json::Value> = (0..3000)
            .map(|n| serde_json::json!({"recordName":format!("target-{n}")}))
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"successor-oversized","moreComing":false,
            "records":[{"recordName":"oversized-master","recordType":"CPLMaster","recordChangeTag":"tag-oversized","fields":{
                "resOriginalRes":{"type":"ASSETID","value":{"size":1000,"downloadURL":"https://p01.icloud-content.com/oversized?token=1","fileChecksum":"checksum-oversized","referenceChecksum":"reference-oversized","wrappingKey":"wrapping-oversized"}},
                ("u".repeat(4000)):{"value":targets}}}]}]})).unwrap();
        assert!(
            body.len() < 16 * 1024 * 1024,
            "the captured raw page fits the inbox limit"
        );
        ObservedPage {
            scope: scope(),
            request_cursor: "cursor-oversized".to_owned(),
            successor: "successor-oversized".to_owned(),
            more_coming: false,
            body,
            identities: vec![SourceIdentity {
                name: "oversized-master".to_owned(),
                record_type: Some("CPLMaster".to_owned()),
                deleted: false,
            }],
        }
    }

    #[tokio::test]
    async fn legacy_compaction_pins_captured_page_whose_projection_exceeds_budget() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oversized-projection.db");
        let (owner, _, _) = legacy_v35(&path);
        let oversized = oversized_unprojected_observed();
        let original_hash = format!("{:x}", Sha256::digest(&oversized.body));
        let id = {
            let conn = Connection::open(&path).unwrap();
            // Captured before projection, exactly as the live capture owner
            // commits the raw page before computing catalogue facts.
            let id = insert_legacy(&conn, &oversized, 200_000);
            let (account_key, provider_key): (String, String) = conn
                .query_row(
                    "SELECT account_key,provider_key FROM account_owner",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let stored = StoredPage {
                id: CapturedPageId(id),
                account_key,
                provider_key,
                observation_hash: original_hash.clone(),
                provenance: PageProvenance::Raw,
                page: oversized.clone(),
            };
            let error = match crate::icloud::photos::plan_retained(&stored) {
                Ok(_) => panic!("the fixture's projection must exceed its byte budget"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("projection is full"),
                "unexpected fixture projection error: {error}"
            );
            assert!(
                !conn
                    .query_row::<bool, _, _>(
                        "SELECT EXISTS(SELECT 1 FROM provider_catalog_pages WHERE page_id=?1)",
                        [id],
                        |r| r.get(0)
                    )
                    .unwrap()
            );
            id
        };
        let db = crate::state::SqliteStateDb::open_owned(&path, &owner)
            .await
            .unwrap();
        let conn = db.acquire_lock("oversized projection oracle").unwrap();
        assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_transport_aliases",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1,
            "unrelated eligible duplicates still compact"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_transport_aliases WHERE page_id=?1 OR canonical_page_id=?1",
                [id],
                |r| r.get(0)
            )
            .unwrap(),
            0,
            "an unprovable projection must not gain an alias or index"
        );
        let (
            body,
            body_hash,
            stored_scope,
            cursor,
            successor,
            more_coming,
            charged,
        ): (Vec<u8>, String, String, String, String, bool, i64) = conn
            .query_row(
                "SELECT body,body_hash,scope,request_cursor,successor,more_coming,charged_bytes FROM provider_shadow_pages WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        assert_eq!(body, oversized.body);
        assert_eq!(body_hash, original_hash);
        assert_eq!(stored_scope, oversized.scope);
        assert_eq!(cursor, oversized.request_cursor);
        assert_eq!(successor, oversized.successor);
        assert_eq!(more_coming, oversized.more_coming);
        assert_eq!(charged, 200_000);
        let stored = load_page(&conn, CapturedPageId(id), 16 * 1024 * 1024).unwrap();
        assert!(matches!(stored.provenance, PageProvenance::Raw));
        assert_eq!(stored.page.body, oversized.body);
        assert_eq!(stored.observation_hash, original_hash);
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "saved"
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT value FROM metadata WHERE key='pending_sync_token:old:PrimarySync'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "debt"
        );
    }

    #[test]
    fn alias_migration_rolls_back_after_first_alias_and_reopens_quietly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollback.db");
        let (_owner, first, second) = legacy_v35(&path);
        {
            let conn = Connection::open(&path).unwrap();
            let before = charged_sum(&conn);
            TEST_ABORT_AFTER_ALIASES.with(|count| count.set(1));
            assert!(crate::state::schema::migrate(&conn).is_err());
            TEST_ABORT_AFTER_ALIASES.with(|count| count.set(0));
            assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 35);
            assert_eq!(charged_sum(&conn), before);
            assert!(!conn.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='provider_transport_aliases')", [], |r|r.get(0)).unwrap());
            assert_eq!(
                conn.query_row::<Vec<u8>, _, _>(
                    "SELECT body FROM provider_shadow_pages WHERE id=2",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                second.body
            );
        }
        {
            let conn = Connection::open(&path).unwrap();
            crate::state::schema::migrate(&conn).unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_transport_aliases",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT body_hash FROM provider_shadow_pages WHERE id=2",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            format!("{:x}", Sha256::digest(&second.body))
        );
        let stored = load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).unwrap();
        assert_eq!(stored.page.body, first.body);
        assert_eq!(
            stored.observation_hash,
            format!("{:x}", Sha256::digest(&second.body))
        );
    }

    fn migrated_legacy(path: &std::path::Path) -> AccountOwner {
        let (owner, _, _) = legacy_v35(path);
        let conn = Connection::open(path).unwrap();
        crate::state::schema::migrate(&conn).unwrap();
        owner
    }

    #[test]
    fn alias_self_chain_and_mutual_cycles_are_refused_promptly() {
        // Self reference.
        {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("self.db");
            let _owner = migrated_legacy(&path);
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA ignore_check_constraints=ON;
                 UPDATE provider_transport_aliases SET canonical_page_id=2 WHERE page_id=2;",
            )
            .unwrap();
            assert!(load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).is_err());
        }
        // Mutual cycle: 2 -> 1 and 1 -> 2.
        {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("mutual.db");
            let _owner = migrated_legacy(&path);
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO provider_transport_aliases(page_id,canonical_page_id,classifier_version,masked_sha,certificate,provenance_hash) VALUES (1,2,1,'a','b','c')",
                [],
            )
            .unwrap();
            assert!(load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).is_err());
        }
        // Chain: 2 -> 1 and 1 -> 8, where 8 is raw.
        {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("chain.db");
            let (_owner, _budget) = full_legacy_v35(&path);
            {
                let conn = Connection::open(&path).unwrap();
                crate::state::schema::migrate(&conn).unwrap();
            }
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "INSERT INTO provider_transport_aliases(page_id,canonical_page_id,classifier_version,masked_sha,certificate,provenance_hash) VALUES (1,8,1,'a','b','c')",
                [],
            )
            .unwrap();
            assert!(load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).is_err());
        }
    }

    #[test]
    fn alias_provenance_binding_detects_each_tamper() {
        let tampers = [
            "UPDATE provider_shadow_pages SET body_hash='tampered' WHERE id=2",
            "UPDATE provider_transport_aliases SET certificate='tampered' WHERE page_id=2",
            "UPDATE provider_transport_aliases SET provenance_hash='tampered' WHERE page_id=2",
            "PRAGMA ignore_check_constraints=ON; UPDATE provider_transport_aliases SET canonical_page_id=2 WHERE page_id=2",
        ];
        for sql in tampers {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("tamper.db");
            let _owner = migrated_legacy(&path);
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(sql).unwrap();
            assert!(
                load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).is_err(),
                "{sql}"
            );
        }
    }

    #[test]
    fn corrupted_work_or_selection_dependency_pins_body_without_drop() {
        for sql in [
            "UPDATE provider_work_receipts SET source_hash='tampered' WHERE page_id=2",
            "UPDATE provider_work_receipts SET confirmation_hash='tampered' WHERE page_id=2",
            "UPDATE provider_selection_sources SET body_hash='tampered' WHERE page_id=2",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("dependency.db");
            let (_owner, _budget) = full_legacy_v35(&path);
            {
                let conn = Connection::open(&path).unwrap();
                conn.execute_batch(sql).unwrap();
                crate::state::schema::migrate(&conn).unwrap();
                assert!(
                    !conn
                        .query_row::<bool, _, _>(
                            "SELECT EXISTS(SELECT 1 FROM provider_transport_aliases WHERE page_id=2)",
                            [],
                            |row| row.get(0)
                        )
                        .unwrap(),
                    "{sql}"
                );
                assert!(
                    conn.query_row::<i64, _, _>(
                        "SELECT length(body) FROM provider_shadow_pages WHERE id=2",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap()
                        > 0,
                    "{sql}"
                );
                let stored = load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).unwrap();
                assert!(matches!(stored.provenance, PageProvenance::Raw));
            }
        }
    }

    #[tokio::test]
    async fn full_inbox_compaction_makes_room_for_new_distinct_page() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("full.db");
        let (owner, budget) = full_legacy_v35(&path);
        let before = {
            let conn = Connection::open(&path).unwrap();
            (
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
                charged_sum(&conn),
            )
        };
        assert_eq!(before.0, 35);
        assert_eq!(u64::try_from(before.1).unwrap(), budget);

        // Opening the database performs the complete migration, including
        // compaction. No schema action or manual charge discount is applied.
        let db = std::sync::Arc::new(
            crate::state::SqliteStateDb::open_owned(&path, &owner)
                .await
                .unwrap(),
        );
        let mut capture = crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "legacy-realm",
        );
        capture.capacity = budget;
        {
            let conn = db.acquire_lock("post migration full inbox").unwrap();
            assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                35
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_aliases",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                30
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_canonical",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                5
            );
            let used = u64::try_from(charged_sum(&conn)).unwrap();
            assert!(used < budget, "compaction must release real budget");
            assert!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_catalog_debt WHERE page_id=2",
                    [],
                    |r| r.get(0)
                )
                .unwrap()
                    > 0
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_work_receipts WHERE page_id=2",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_selection_sources WHERE page_id=2",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
            assert_eq!(
                conn.query_row::<String, _, _>(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                "saved"
            );
            let alias = load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).unwrap();
            assert!(matches!(alias.provenance, PageProvenance::LegacyAlias(_)));
        }

        let new_page = fixture_observed(99, "https://p01.icloud-content.com/asset-new?token=0");
        capture.capture(new_page).await.unwrap();
        let after_new = {
            let conn = db.acquire_lock("new page under same budget").unwrap();
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                36
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_aliases",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                30
            );
            assert_eq!(
                charged_sum(&conn),
                conn.query_row::<i64, _, _>(
                    "SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages",
                    [],
                    |r| r.get(0)
                )
                .unwrap()
            );
            charged_sum(&conn)
        };
        assert!(u64::try_from(after_new).unwrap() <= budget);

        let repeat = fixture_observed(99, "https://p09.icloud-content.com/rotated/path?token=1");
        capture.capture(repeat).await.unwrap();
        {
            let conn = db.acquire_lock("stable repeat under same budget").unwrap();
            assert_eq!(charged_sum(&conn), after_new);
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                36
            );
        }
        drop(capture);
        drop(db);
        let reopened = crate::state::SqliteStateDb::open_owned(&path, &owner)
            .await
            .unwrap();
        let conn = reopened.acquire_lock("reopened full inbox").unwrap();
        assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
        assert_eq!(charged_sum(&conn), after_new);
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_transport_aliases",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            30
        );
        assert!(matches!(
            load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024)
                .unwrap()
                .provenance,
            PageProvenance::LegacyAlias(_)
        ));
    }

    async fn actual_inbox_migration_and_catalog_child() {
        let history =
            std::path::PathBuf::from(std::env::var_os("KEI_TEST_ACTUAL_INBOX_HISTORY").unwrap());
        let (_recorder, guard) =
            crate::support::history::start(history, serde_json::json!({"threads": 1})).unwrap();
        let directory = tempfile::tempdir().unwrap();

        let commit_path = directory.path().join("commit.db");
        let (owner, _, _) = legacy_v35(&commit_path);
        let before_charge = {
            let conn = Connection::open(&commit_path).unwrap();
            charged_sum(&conn)
        };
        {
            let conn = Connection::open(&commit_path).unwrap();
            crate::state::schema::migrate(&conn).unwrap();
        }
        let after_charge = {
            let conn = Connection::open(&commit_path).unwrap();
            charged_sum(&conn)
        };
        assert!(after_charge < before_charge);

        crate::support::begin(serde_json::json!({"scenario": "inbox-new-capacity"}));
        let db = std::sync::Arc::new(
            crate::state::SqliteStateDb::open_owned(&commit_path, &owner)
                .await
                .unwrap(),
        );
        let mut capture = crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "legacy-realm",
        );
        capture.capacity = 512 * 1024 * 1024;
        let fresh = fixture_observed(60, "https://p01.icloud-content.com/asset-60?token=1");
        capture.capture(fresh.clone()).await.unwrap();
        capture.capacity = 0;
        let refused = fixture_observed(61, "https://p01.icloud-content.com/asset-61?token=2");
        assert!(capture.capture(refused).await.is_err());
        drop(capture);
        drop(db);

        crate::support::begin(serde_json::json!({"scenario": "inbox-reuse-unknown"}));
        let db = std::sync::Arc::new(
            crate::state::SqliteStateDb::open_owned(&commit_path, &owner)
                .await
                .unwrap(),
        );
        let mut capture = crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "legacy-realm",
        );
        capture.capacity = 512 * 1024 * 1024;
        capture.capture(fresh).await.unwrap();
        let equivalent = fixture_observed(60, "https://p01.icloud-content.com/asset-60?token=9");
        capture.capture(equivalent).await.unwrap();
        drop(capture);
        drop(db);

        crate::support::begin(serde_json::json!({"scenario": "failed-alias"}));
        let failed_path = directory.path().join("failed.db");
        let (_owner, _, _) = legacy_v35(&failed_path);
        {
            let conn = Connection::open(&failed_path).unwrap();
            TEST_ABORT_AFTER_ALIASES.with(|count| count.set(1));
            assert!(crate::state::schema::migrate(&conn).is_err());
            TEST_ABORT_AFTER_ALIASES.with(|count| count.set(0));
        }

        crate::support::begin(serde_json::json!({"scenario": "reopen-commit"}));
        {
            let conn = Connection::open(&failed_path).unwrap();
            crate::state::schema::migrate(&conn).unwrap();
        }

        crate::support::begin(serde_json::json!({"scenario": "unprojectable"}));
        let unprojectable_path = directory.path().join("unprojectable.db");
        let (_owner, _, _) = legacy_v35(&unprojectable_path);
        {
            let conn = Connection::open(&unprojectable_path).unwrap();
            insert_legacy(&conn, &oversized_unprojected_observed(), 200_000);
            crate::state::schema::migrate(&conn).unwrap();
        }

        crate::support::begin(serde_json::json!({"scenario": "inbox-catalogue-single-item-limit"}));
        let db = std::sync::Arc::new(
            crate::state::SqliteStateDb::open_owned(&commit_path, &owner)
                .await
                .unwrap(),
        );
        let mut capture = crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "legacy-realm",
        );
        capture.capacity = 512 * 1024 * 1024;
        let oversized = oversized_unprojected_observed();
        let error = capture.capture(oversized.clone()).await.unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::icloud::photos::error::ShadowPageError>()
                .is_some(),
            "normal capture preserves the observation-refusal wrapper"
        );
        let retained = db
            .next_catalog_source(owner.clone(), 16 * 1024 * 1024)
            .await
            .unwrap()
            .expect("the oversized raw page remains captured");
        assert_eq!(retained.page.body, oversized.body);
        let projection_error = crate::icloud::photos::plan_retained(&retained)
            .err()
            .expect("normalization exceeds its existing bound");
        assert!(matches!(
            projection_error.downcast_ref::<crate::state::error::StateError>(),
            Some(crate::state::error::StateError::ProviderCatalogFull)
        ));
        let projections: i64 = {
            let conn = db
                .acquire_lock("verify refused projection retained no catalogue commit")
                .unwrap();
            conn.query_row(
                "SELECT count(*) FROM provider_catalog_pages WHERE page_id=?1",
                [retained.id.0],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(projections, 0);
        drop(capture);
        crate::support::begin(serde_json::json!({"scenario": "catalogue"}));
        let fresh_catalog = fixture_observed(70, "https://p01.icloud-content.com/asset-70?token=1");
        let fresh_id = {
            let conn = db.acquire_lock("insert real catalogue source").unwrap();
            insert_legacy(&conn, &fresh_catalog, 20_000)
        };
        let source = db
            .catalog_source(owner.clone(), CapturedPageId(fresh_id), 16 * 1024 * 1024)
            .await
            .unwrap();
        let plan = crate::icloud::photos::plan_retained(&source).unwrap();
        db.project_catalog_page(owner.clone(), plan, 512 * 1024 * 1024)
            .await
            .unwrap();
        let source = db
            .catalog_source(owner.clone(), CapturedPageId(fresh_id), 16 * 1024 * 1024)
            .await
            .unwrap();
        let plan = crate::icloud::photos::plan_retained(&source).unwrap();
        db.project_catalog_page(owner.clone(), plan, 0)
            .await
            .unwrap();
        let refused_catalog =
            fixture_observed(71, "https://p01.icloud-content.com/asset-71?token=2");
        let refused_id = {
            let conn = db.acquire_lock("insert refused catalogue source").unwrap();
            insert_legacy(&conn, &refused_catalog, 20_000)
        };
        let source = db
            .catalog_source(owner.clone(), CapturedPageId(refused_id), 16 * 1024 * 1024)
            .await
            .unwrap();
        let plan = crate::icloud::photos::plan_retained(&source).unwrap();
        assert!(matches!(
            db.project_catalog_page(owner.clone(), plan, 0).await,
            Err(crate::state::error::StateError::ProviderCatalogFull)
        ));
        crate::support::begin(serde_json::json!({}));
        let mut changed = fixture_observed(60, "https://p01.icloud-content.com/asset-60?token=9");
        let mut value: serde_json::Value = serde_json::from_slice(&changed.body).unwrap();
        value["newProviderField"] = serde_json::json!("synthetic");
        changed.body = serde_json::to_vec(&value).unwrap();
        db.capture_shadow_page(owner.clone(), changed.clone(), 512 * 1024 * 1024)
            .await
            .unwrap();
        for _ in 0..10 {
            db.capture_shadow_page(owner.clone(), changed.clone(), 0)
                .await
                .unwrap();
        }
        drop(db);
        guard.finish().await;
    }

    #[tokio::test]
    async fn actual_inbox_migration_and_catalog_budget_record_real_owners() {
        if std::env::var_os("KEI_TEST_ACTUAL_INBOX_CHILD").is_some() {
            actual_inbox_migration_and_catalog_child().await;
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let history = root.path().join("support.json");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "state::db::provider_inbox::tests::actual_inbox_migration_and_catalog_budget_record_real_owners",
                "--nocapture",
            ])
            .env("TMPDIR", root.path())
            .env("TMP", root.path())
            .env("TEMP", root.path())
            .env("KEI_TEST_ACTUAL_INBOX_CHILD", "1")
            .env("KEI_TEST_ACTUAL_INBOX_HISTORY", &history)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshot = crate::support::history::test_retained_snapshot(&history);
        let cycles = snapshot["cycles"].as_array().unwrap();
        let committed = &cycles[0]["transport_compaction"];
        assert_eq!(committed["committed"], 1);
        assert_eq!(committed["reason"], "committed");
        let before = committed["logical_charge_before"].as_u64().unwrap();
        let after = committed["logical_charge_after"].as_u64().unwrap();
        assert!(before > after);
        assert_eq!(committed["committed_reclaim"], before - after);
        let inbox_new = &cycles[1]["storage_reuse"]["inbox"];
        assert_eq!(inbox_new["new"], 1);
        assert_eq!(inbox_new["capacity_rejected"], 1);
        assert_eq!(inbox_new["last_reason"], "capacity");
        assert_eq!(inbox_new["last_observed_limit"], 0);
        let inbox_reuse = &cycles[2]["storage_reuse"]["inbox"];
        assert_eq!(inbox_reuse["exact"], 1);
        assert_eq!(inbox_reuse["equivalent"], 1);
        assert_eq!(inbox_reuse["last_reason"], "none");
        assert!(inbox_reuse["last_observed_used"].is_null());
        let failed = &cycles[3]["transport_compaction"];
        assert_eq!(failed["failed"], 1);
        assert_eq!(failed["reason"], "migration_failed");
        assert_eq!(failed["committed_reclaim"], 0);
        assert!(failed["logical_charge_before"].is_null());
        let reopened = &cycles[4]["transport_compaction"];
        assert_eq!(reopened["committed"], 1);
        assert_eq!(reopened["reason"], "committed");
        assert!(reopened["committed_reclaim"].as_u64().unwrap() > 0);
        let unprojectable = &cycles[5]["transport_compaction"];
        assert!(
            unprojectable["retained_projection_or_dependency"]
                .as_u64()
                .unwrap()
                >= 1
        );
        assert_eq!(cycles[5]["storage_reuse"]["catalogue"]["observed"], false);
        let normal_capacity = &cycles[6]["storage_reuse"]["catalogue"];
        assert_eq!(normal_capacity["single_item_limit"], 1);
        assert_eq!(normal_capacity["new"], 0);
        let catalogue = &cycles[7]["storage_reuse"]["catalogue"];
        assert_eq!(catalogue["new"], 1);
        assert_eq!(catalogue["exact"], 1);
        assert_eq!(catalogue["capacity_rejected"], 1);
        assert_eq!(catalogue["last_reason"], "capacity");
        assert_eq!(catalogue["last_observed_limit"], 0);
        let unknown = &cycles[8]["storage_reuse"]["inbox"];
        assert_eq!(unknown["new"], 1);
        assert_eq!(unknown["exact"], 10);
        assert_eq!(unknown["equivalent"], 0);
        assert_eq!(unknown["observed"], true);
    }
}
