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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoredPage {
    pub(crate) id: CapturedPageId,
    pub(crate) account_key: String,
    pub(crate) provider_key: String,
    /// The exact hash observed when this durable identity was first admitted.
    /// This is the SHA-256 of `page.body`, validated on every load.
    pub(crate) observation_hash: String,
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
    if observation_hash != format!("{:x}", Sha256::digest(&body)) {
        return Err(StateError::ProviderCatalogInvalid);
    }
    Ok(StoredPage {
        id,
        account_key,
        provider_key,
        observation_hash,
        page: ObservedPage {
            scope,
            request_cursor,
            successor,
            more_coming: more == 1,
            body,
            identities: stored_identities(conn, id.0)?,
        },
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

/// Bind a completed handoff to the immutable source and its validated projection.
/// The signed URLs themselves are no longer required to recognize an equivalent
/// observation: the versioned canonical key includes the masked body digest.
pub(super) fn release_binding(conn: &Connection, id: i64) -> Result<String, StateError> {
    let header: String = conn.query_row("SELECT json_array(p.account_key,p.provider_key,p.scope,p.request_cursor,p.successor,p.more_coming,p.body_hash,c.projector_version,c.record_count,c.reference_count,c.debt_count,c.charged_bytes,t.canonical_sha,t.version)
        FROM provider_shadow_pages p JOIN provider_catalog_pages c ON c.page_id=p.id AND c.body_hash=p.body_hash AND c.projector_version=1
        LEFT JOIN provider_transport_canonical t ON t.page_id=p.id WHERE p.id=?1", [id], |r|r.get(0))?;
    let mut digest = Sha256::new();
    digest.update(header.as_bytes());
    for identity in stored_identities(conn, id)? {
        let bytes = serde_json::to_vec(&(identity.name, identity.record_type, identity.deleted))
            .map_err(|_invalid| StateError::ProviderCatalogInvalid)?;
        digest.update(bytes);
    }
    // The receipt also replaces replay-time validation of the catalog against
    // the raw page. Detect mutations to facts even if row counts stay equal.
    for query in [
        "SELECT json_array(ordinal,record_name,record_type,deleted,kind) FROM provider_catalog_records WHERE page_id=?1 ORDER BY ordinal",
        "SELECT json_array(ordinal,field_path,target_record_name,target_zone_name,target_zone_owner) FROM provider_catalog_references WHERE page_id=?1 ORDER BY ordinal,field_path,target_record_name",
        "SELECT json_array(ordinal,field_path,reason) FROM provider_catalog_debt WHERE page_id=?1 ORDER BY ordinal,field_path,reason",
    ] {
        for fact in conn
            .prepare(query)?
            .query_map([id], |row| row.get::<_, String>(0))?
        {
            let fact = fact?;
            digest.update((fact.len() as u64).to_le_bytes());
            digest.update(fact.as_bytes());
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub(super) fn released(conn: &Connection, id: i64) -> Result<bool, StateError> {
    let receipt: Option<(String,i64)> = conn.query_row("SELECT f.receipt,length(p.body) FROM provider_finished_changes f JOIN provider_shadow_pages p ON p.id=f.page_id WHERE f.page_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((receipt, length)) = receipt else {
        return Ok(false);
    };
    if length != 0 || receipt != release_binding(conn, id)? {
        return Err(StateError::ProviderCatalogInvalid);
    }
    Ok(true)
}

fn checked_released_replay(
    conn: &Connection,
    id: i64,
    page: &ObservedPage,
) -> Result<bool, StateError> {
    if !released(conn, id)? {
        return Ok(false);
    }
    let matches:bool=conn.query_row("SELECT account_key=(SELECT account_key FROM account_owner WHERE singleton=1) AND provider_key=(SELECT provider_key FROM account_owner WHERE singleton=1) AND scope=?2 AND request_cursor=?3 AND successor=?4 AND more_coming=?5 FROM provider_shadow_pages WHERE id=?1",params![id,page.scope,page.request_cursor,page.successor,page.more_coming],|r|r.get(0))?;
    if !matches || stored_identities(conn, id)? != page.identities {
        return Err(StateError::ProviderInboxInvalid);
    }
    Ok(true)
}

impl SqliteStateDb {
    #[cfg(test)]
    pub(crate) async fn capture_shadow_page(
        &self,
        owner: account::AccountOwner,
        page: impl Into<std::sync::Arc<ObservedPage>>,
        capacity: u64,
    ) -> Result<CapturedPageId, StateError> {
        self.attempt_shadow_capture(owner, page.into(), capacity, true)
            .await
    }

    pub(crate) async fn attempt_shadow_capture(
        &self,
        owner: account::AccountOwner,
        page: std::sync::Arc<ObservedPage>,
        capacity: u64,
        final_attempt: bool,
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
                if checked_released_replay(&tx,id,&page)? {return Ok(CapturedPageId(id))}
                let original = load_page(
                    &tx,
                    CapturedPageId(id),
                    crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
                )?;
                if original.page.body != page.body
                    || !same_capture_metadata(&original.page, &page)
                {
                    return Err(StateError::ProviderInboxInvalid);
                }
                storage = (StorageOutcome::Exact, None);
                id
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
                if let Some((representative_id, stored_version)) = canonical_hit {
                    let (canonical_sha, version) =
                        canonical.as_ref().ok_or(StateError::ProviderInboxInvalid)?;
                    if stored_version==i64::from(*version) && checked_released_replay(&tx,representative_id,&page)? {return Ok(CapturedPageId(representative_id))}
                    let representative = load_page(
                        &tx,
                        CapturedPageId(representative_id),
                        crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES,
                    )?;
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
                        "SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages p WHERE NOT EXISTS(SELECT 1 FROM provider_finished_changes f WHERE f.page_id=p.id)", [], |row| row.get(0),
                    )?;
                    let used = u64::try_from(used).map_err(|_error| StateError::ProviderInboxInvalid)?;
                    if charge > capacity.saturating_sub(used) {
                        if final_attempt {
                        crate::support::record_storage(
                            StorageComponent::Inbox,
                            StorageOutcome::CapacityRejected,
                            Some((used, capacity)),
                        );
                        }
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
            tx.commit()?;
            crate::support::record_storage(StorageComponent::Inbox, storage.0, storage.1);
            Ok(CapturedPageId(page_id))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::{CapturedPageId, ObservedPage, SourceIdentity, StoredPage, load_page};
    use crate::state::db::account::AccountOwner;
    use crate::test_helpers::{assert_no_retired_inbox_archive, single_recovery_bundle_connection};
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
            "SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages",
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
    async fn fresh_start_archives_alias_bytes_then_captures_current_raw_pages() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.db");
        let (owner, first, second) = legacy_v35(&path);
        let alias_rows = |conn: &Connection, table: &str| {
            conn.prepare(&format!("SELECT * FROM {table} ORDER BY page_id"))
                .unwrap()
                .query_map([], |row| {
                    (0..6)
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let before = {
            let conn = Connection::open(&path).unwrap();
            for version in 36..=37 {
                crate::state::schema::migrate_to_version(&conn, 35, version).unwrap();
            }
            // Unsupported historical certificate bytes must never be interpreted.
            conn.execute_batch("INSERT INTO provider_transport_canonical VALUES ('old-certificate',99,1);
                INSERT INTO provider_transport_aliases VALUES (2,1,99,'old-mask','old-certificate',X'00ff');
                UPDATE provider_shadow_pages SET body=X'' WHERE id=2;").unwrap();
            alias_rows(&conn, "provider_transport_aliases")
        };
        for cycle in 0..3 {
            let db = crate::state::SqliteStateDb::open_owned(&path, &owner)
                .await
                .unwrap();
            let id = db
                .capture_shadow_page(
                    owner.clone(),
                    second.clone(),
                    if cycle == 0 { 512 * 1024 * 1024 } else { 0 },
                )
                .await
                .unwrap();
            assert_eq!(id.0, 1);
            assert_eq!(
                db.capture_shadow_page(owner.clone(), first.clone(), 0)
                    .await
                    .unwrap(),
                id
            );
            let conn = db
                .acquire_lock("archived aliases and current raw capture")
                .unwrap();
            assert_eq!(
                alias_rows(&conn, "historical_v030_provider_transport_aliases"),
                before
            );
            assert!(alias_rows(&conn, "provider_transport_aliases").is_empty());
            assert_no_retired_inbox_archive(&conn);
            let original = single_recovery_bundle_connection(directory.path());
            assert_eq!(
                original
                    .query_row::<Vec<u8>, _, _>(
                        "SELECT body FROM provider_shadow_pages WHERE id=1",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                first.body
            );
            assert_eq!(
                original
                    .query_row::<Vec<u8>, _, _>(
                        "SELECT body FROM provider_shadow_pages WHERE id=2",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                Vec::<u8>::new()
            );
            assert_eq!(
                original
                    .query_row::<String, _, _>(
                        "SELECT body_hash FROM provider_shadow_pages WHERE id=2",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                format!("{:x}", Sha256::digest(&second.body))
            );
            assert_eq!(load_page(&conn, id, 16 * 1024 * 1024).unwrap().page, second);
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                1
            );
        }
        let db = crate::state::SqliteStateDb::open_owned(&path, &owner)
            .await
            .unwrap();
        db.acquire_lock("corrupt current bytes")
            .unwrap()
            .execute("UPDATE provider_shadow_pages SET body=X'00' WHERE id=1", [])
            .unwrap();
        assert!(
            db.capture_shadow_page(owner, second, 0).await.is_err(),
            "current raw hash validation must still refuse corruption"
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
    async fn fresh_start_preserves_history_whose_projection_exceeds_budget() {
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
        assert_eq!(
            crate::state::schema::get_schema_version(&conn).unwrap(),
            crate::state::schema::SCHEMA_VERSION
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_transport_aliases",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            0,
            "historical pages must not create current aliases"
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
        assert_no_retired_inbox_archive(&conn);
        let original = single_recovery_bundle_connection(directory.path());
        let (
            body,
            body_hash,
            stored_scope,
            cursor,
            successor,
            more_coming,
            charged,
        ): (Vec<u8>, String, String, String, String, bool, i64) = original
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
        assert!(load_page(&conn, CapturedPageId(id), 16 * 1024 * 1024).is_err());
        crate::test_helpers::assert_checkpoint_evidence(&conn, "sync_token:PrimarySync", "saved");
        crate::test_helpers::assert_checkpoint_evidence(
            &conn,
            "pending_sync_token:old:PrimarySync",
            "debt",
        );
    }

    #[tokio::test]
    async fn fresh_start_preserves_corrupted_work_dependencies_as_history() {
        for sql in [
            "UPDATE provider_work_receipts SET source_hash='tampered' WHERE page_id=2",
            "UPDATE provider_work_receipts SET confirmation_hash='tampered' WHERE page_id=2",
            "UPDATE provider_selection_sources SET body_hash='tampered' WHERE page_id=2",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("dependency.db");
            let (owner, _budget) = full_legacy_v35(&path);
            let body: Vec<u8> = {
                let conn = Connection::open(&path).unwrap();
                conn.execute_batch(sql).unwrap();
                conn.query_row(
                    "SELECT body FROM provider_shadow_pages WHERE id=2",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
            };
            let db = crate::state::SqliteStateDb::open_owned(&path, &owner)
                .await
                .unwrap();
            let conn = db
                .acquire_lock("corrupted work dependencies retired")
                .unwrap();
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
            assert_no_retired_inbox_archive(&conn);
            let original = single_recovery_bundle_connection(directory.path());
            let archived: Vec<u8> = original
                .query_row(
                    "SELECT body FROM provider_shadow_pages WHERE id=2",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!archived.is_empty(), "{sql}");
            assert_eq!(archived, body, "{sql}");
            assert!(load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).is_err());
        }
    }

    #[tokio::test]
    async fn fresh_start_full_inbox_retirement_allows_current_capture_and_quiet_reopen() {
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

        let db = std::sync::Arc::new(
            crate::state::SqliteStateDb::open_owned(&path, &owner)
                .await
                .unwrap(),
        );
        let original = single_recovery_bundle_connection(directory.path());
        let mut capture = crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "legacy-realm",
        );
        capture.capacity = budget;
        {
            let conn = db.acquire_lock("post migration full inbox").unwrap();
            assert_eq!(
                original
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM provider_shadow_pages",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                before.0
            );
            assert_eq!(
                original
                    .query_row::<i64, _, _>(
                        "SELECT sum(charged_bytes) FROM provider_shadow_pages",
                        [],
                        |row| row.get(0)
                    )
                    .unwrap(),
                before.1
            );
            assert_no_retired_inbox_archive(&conn);
            assert_eq!(
                crate::state::schema::get_schema_version(&conn).unwrap(),
                crate::state::schema::SCHEMA_VERSION
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_shadow_pages",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_aliases",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_canonical",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0
            );
            let used = u64::try_from(charged_sum(&conn)).unwrap();
            assert_eq!(used, 0, "retired pages must not consume current capacity");
            assert!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM historical_v030_provider_catalog_debt WHERE page_id=2",
                    [],
                    |r| r.get(0)
                )
                .unwrap()
                    > 0
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM historical_v030_provider_work_receipts WHERE page_id=2",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM historical_v030_provider_selection_sources WHERE page_id=2",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
            crate::test_helpers::assert_checkpoint_evidence(
                &conn,
                "sync_token:PrimarySync",
                "saved",
            );
            assert!(load_page(&conn, CapturedPageId(2), 16 * 1024 * 1024).is_err());
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
                1
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_transport_aliases",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                0
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
                1
            );
        }
        drop(capture);
        drop(db);
        let reopened = crate::state::SqliteStateDb::open_owned(&path, &owner)
            .await
            .unwrap();
        let conn = reopened.acquire_lock("reopened full inbox").unwrap();
        assert_eq!(
            crate::state::schema::get_schema_version(&conn).unwrap(),
            crate::state::schema::SCHEMA_VERSION
        );
        assert_no_retired_inbox_archive(&conn);
        assert_eq!(charged_sum(&conn), after_new);
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_transport_aliases",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
        let stored = load_page(&conn, CapturedPageId(1), 16 * 1024 * 1024).unwrap();
        assert_eq!(
            stored.observation_hash,
            format!("{:x}", Sha256::digest(&stored.page.body))
        );
    }

    /// Reuse the base private master fixture as an eligible Live Photo motion
    /// descriptor. Only the descriptor field and its checksum change.
    fn motion_observed(url: &str, checksum: &str) -> ObservedPage {
        let mut page = observed(url);
        page.body = String::from_utf8(page.body)
            .unwrap()
            .replace("resOriginalRes", "resOriginalVidComplRes")
            .replace("checksum-A", checksum)
            .into_bytes();
        page
    }

    fn inbox_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM provider_shadow_pages", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[tokio::test]
    async fn consumed_tombstones_release_space_mid_stream_and_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handoff.db");
        let owner = owner_record();
        let tombstone = |index: usize| {
            let name = format!("deleted-{index}");
            let raw = serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":[{"recordName":name,"recordType":"CPLAsset","deleted":true}],"syncToken":"successor","moreComing":false}]});
            crate::icloud::photos::catalog_observed_page(
                serde_json::to_vec(&raw).unwrap(),
                &scope(),
                "held",
            )
            .unwrap()
        };
        let unresolved = observed("https://p01.icloud-content.com/unknown");
        let db = std::sync::Arc::new(
            crate::state::SqliteStateDb::open_owned(&path, &owner)
                .await
                .unwrap(),
        );
        db.set_metadata("sync_token:PrimarySync", "held")
            .await
            .unwrap();
        let mut capture = crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            owner.clone(),
            "legacy-realm",
        );
        capture.capture(unresolved.clone()).await.unwrap();
        capture.capture(tombstone(0)).await.unwrap();
        capture.capacity =
            u64::try_from(charged_sum(&db.acquire_lock("initial budget").unwrap())).unwrap();
        {
            let conn = db
                .acquire_lock("apply deletion while checkpoint held")
                .unwrap();
            for index in 0..8 {
                conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,is_deleted) VALUES('PrimarySync',?1,'original','checksum','old.jpg',1,10,'photo','downloaded',1,1)",[format!("deleted-{index}")]).unwrap();
            }
            conn.execute_batch("CREATE TRIGGER fail_release BEFORE UPDATE OF body ON provider_shadow_pages BEGIN SELECT RAISE(ABORT,'injected cleanup failure'); END;").unwrap();
        }
        assert!(capture.capture(tombstone(1)).await.is_err());
        {
            let conn = db.acquire_lock("release rolled back").unwrap();
            assert_eq!(
                conn.query_row("SELECT count(*) FROM provider_finished_changes", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
                0
            );
            assert_eq!(
                load_page(&conn, CapturedPageId(2), 16384).unwrap().page,
                tombstone(0)
            );
            conn.execute_batch("DROP TRIGGER fail_release").unwrap();
        }
        for index in 1..8 {
            capture.capture(tombstone(index)).await.unwrap();
        }
        drop(capture);
        drop(db);
        let db = std::sync::Arc::new(
            crate::state::SqliteStateDb::open_owned(&path, &owner)
                .await
                .unwrap(),
        );
        let mut capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "legacy-realm");
        capture.capacity = 0;
        capture.capture(tombstone(0)).await.unwrap();
        assert!(capture.capture(tombstone(8)).await.is_err());
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("held")
        );
        let conn = db.acquire_lock("durable handoff").unwrap();
        assert_eq!(
            load_page(&conn, CapturedPageId(1), 16384).unwrap().page,
            unresolved
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM provider_finished_changes f JOIN provider_shadow_pages p ON p.id=f.page_id WHERE length(p.body)=0",[],|r|r.get::<_,i64>(0)).unwrap(),8);
    }

    #[tokio::test]
    async fn motion_url_rotation_is_free_at_capacity_but_a_changed_motion_checksum_is_new_evidence()
    {
        const ORIGINAL_URL: &str = "https://p01.icloud-content.com/motion?token=0";
        const ROTATED_URLS: [&str; 3] = [
            "https://p99.icloud-content.com/rotated/motion?token=1",
            "https://p02.cdn-apple.com/motion?token=2",
            "https://p01.icloud-content.com/another/path?token=3",
        ];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("motion.db");
        let owner = owner_record();
        let original = motion_observed(ORIGINAL_URL, "checksum-A");

        // Cycle 0 captures the baseline; every later cycle reopens the owned
        // file to replay only the signed URL, then refuse and finally retain
        // the changed motion checksum.
        let mut baseline: Option<(i64, i64)> = None;
        for cycle in 0..3 {
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
            let Some((rows, charged)) = baseline else {
                capture.capacity = 512 * 1024 * 1024;
                capture.capture(original.clone()).await.unwrap();
                let conn = db.acquire_lock("captured private motion page").unwrap();
                let rows = inbox_rows(&conn);
                let charged = charged_sum(&conn);
                assert_eq!(rows, 1);
                assert!(charged > 0);
                baseline = Some((rows, charged));
                continue;
            };

            capture.capacity = u64::try_from(charged).unwrap();
            for url in ROTATED_URLS {
                capture
                    .capture(motion_observed(url, "checksum-A"))
                    .await
                    .unwrap();
            }
            {
                let conn = db.acquire_lock("replayed signed motion URL").unwrap();
                assert_eq!(inbox_rows(&conn), rows);
                assert_eq!(charged_sum(&conn), charged);
            }
            if cycle < 2 {
                continue;
            }

            // A changed motion checksum is different provider evidence even
            // with an identical cursor, scope and continuation.
            let changed = motion_observed(ORIGINAL_URL, "checksum-B");
            let error = capture.capture(changed.clone()).await.unwrap_err();
            let refusal = error
                .downcast_ref::<crate::icloud::photos::error::ShadowPageError>()
                .expect("a refused observation keeps its typed wrapper");
            assert_eq!(
                refusal.to_string(),
                crate::state::error::StateError::ProviderInboxFull.to_string(),
                "new motion evidence must be refused for capacity: {error:#}"
            );
            {
                let conn = db.acquire_lock("changed motion checksum refused").unwrap();
                assert_eq!(inbox_rows(&conn), rows);
                assert_eq!(charged_sum(&conn), charged);
            }

            // Adding headroom retains both pieces of evidence side by side.
            capture.capacity = u64::try_from(charged).unwrap() + 64 * 1024;
            capture.capture(changed.clone()).await.unwrap();
            let conn = db.acquire_lock("changed motion checksum retained").unwrap();
            assert_eq!(inbox_rows(&conn), 2);
            assert!(charged_sum(&conn) > charged);
            let bodies = conn
                .prepare("SELECT body FROM provider_shadow_pages ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(bodies, vec![original.body.clone(), changed.body]);
        }
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
        assert_eq!(committed["committed"], 0);
        assert_eq!(committed["failed"], 0);
        assert_eq!(committed["reason"], "not_observed");
        assert_eq!(committed["compacted_aliases"], 0);
        assert!(committed["committed_reclaim"].is_null());
        assert!(committed["logical_charge_before"].is_null());
        assert!(committed["logical_charge_after"].is_null());
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
        let normal_capacity = &cycles[3]["storage_reuse"]["catalogue"];
        assert_eq!(normal_capacity["single_item_limit"], 1);
        assert_eq!(normal_capacity["new"], 0);
        let catalogue = &cycles[4]["storage_reuse"]["catalogue"];
        assert_eq!(catalogue["new"], 1);
        assert_eq!(catalogue["exact"], 1);
        assert_eq!(catalogue["capacity_rejected"], 1);
        assert_eq!(catalogue["last_reason"], "capacity");
        assert_eq!(catalogue["last_observed_limit"], 0);
        let unknown = &cycles[5]["storage_reuse"]["inbox"];
        assert_eq!(unknown["new"], 1);
        assert_eq!(unknown["exact"], 10);
        assert_eq!(unknown["equivalent"], 0);
        assert_eq!(unknown["observed"], true);
    }
}
