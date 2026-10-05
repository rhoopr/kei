//! Additive observations; these receipts never authorize a legacy checkpoint.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::{SqliteStateDb, account};
use crate::state::error::StateError;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    pub(crate) name: String,
    pub(crate) record_type: Option<String>,
    pub(crate) deleted: bool,
}

/// Constructed by the provider adapter only after complete page validation.
#[derive(Debug, PartialEq, Eq)]
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

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StoredPage {
    pub(crate) id: CapturedPageId,
    pub(crate) account_key: String,
    pub(crate) provider_key: String,
    pub(crate) body_hash: String,
    pub(crate) page: ObservedPage,
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
    let (account_key, provider_key, scope, request_cursor, successor, more, body_hash, body):
        (String,String,String,String,String,i64,String,Vec<u8>) = conn.query_row(
        "SELECT account_key,provider_key,scope,request_cursor,successor,more_coming,body_hash,body FROM provider_shadow_pages WHERE id=?1",
        [id.0], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?)),
    )?;
    let owner: (String, String) = conn.query_row(
        "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if (&account_key, &provider_key) != (&owner.0, &owner.1)
        || !matches!(more, 0 | 1)
        || body_hash != format!("{:x}", Sha256::digest(&body))
    {
        return Err(StateError::ProviderCatalogInvalid);
    }
    let identities = conn.prepare(
        "SELECT ordinal,record_name,record_type,deleted FROM provider_shadow_records WHERE page_id=?1 ORDER BY ordinal",
    )?.query_map([id.0], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,
        row.get::<_,Option<String>>(2)?,row.get::<_,i64>(3)?)))?.collect::<Result<Vec<_>,_>>()?;
    for (expected, (ordinal, _, _, deleted)) in identities.iter().enumerate() {
        if i64::try_from(expected).ok() != Some(*ordinal) || !matches!(deleted, 0 | 1) {
            return Err(StateError::ProviderCatalogInvalid);
        }
    }
    Ok(StoredPage {
        id,
        account_key,
        provider_key,
        body_hash,
        page: ObservedPage {
            scope,
            request_cursor,
            successor,
            more_coming: more == 1,
            body,
            identities: identities
                .into_iter()
                .map(|(_, name, record_type, deleted)| SourceIdentity {
                    name,
                    record_type,
                    deleted: deleted == 1,
                })
                .collect(),
        },
    })
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
            let existing: Option<(i64, Vec<u8>)> = tx.query_row(
                "SELECT id,body FROM provider_shadow_pages WHERE scope=?1 AND request_cursor=?2 AND body_hash=?3",
                params![page.scope, page.request_cursor, hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            let page_id = if let Some((id, original)) = existing {
                if original != page.body {
                    return Err(StateError::ProviderInboxInvalid);
                }
                id
            } else {
                // Charge UTF-8 payload and provenance bytes, including source
                // identities. This is an inbox budget, not a disk-file quota.
                let charge = page.body.len() as u64 + page.scope.len() as u64
                    + page.request_cursor.len() as u64 + page.successor.len() as u64
                    + account_key.len() as u64 + provider_key.len() as u64 + 128
                    + page.identities.iter().map(|id| id.name.len() as u64
                        + id.record_type.as_ref().map_or(0, |kind| kind.len() as u64) + 32).sum::<u64>();
                let used: i64 = tx.query_row(
                    "SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages", [], |row| row.get(0),
                )?;
                let used = u64::try_from(used).map_err(|_error| StateError::ProviderInboxInvalid)?;
                if charge > capacity.saturating_sub(used) {
                    return Err(StateError::ProviderInboxFull);
                }
                let charge = i64::try_from(charge).map_err(|_error| StateError::ProviderInboxFull)?;
                tx.execute(
                    "INSERT INTO provider_shadow_pages(account_key,provider_key,scope,request_cursor,successor,more_coming,body_hash,body,charged_bytes,observed_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![account_key, provider_key, page.scope, page.request_cursor,
                        page.successor, page.more_coming, hash, page.body, charge, chrono::Utc::now().timestamp()],
                )?;
                let id = tx.last_insert_rowid();
                for (ordinal, identity) in page.identities.iter().enumerate() {
                    let ordinal = i64::try_from(ordinal).map_err(|_error| StateError::ProviderInboxInvalid)?;
                    tx.execute(
                        "INSERT INTO provider_shadow_records(page_id,ordinal,record_name,record_type,deleted) VALUES (?1,?2,?3,?4,?5)",
                        params![id, ordinal, identity.name, identity.record_type, identity.deleted],
                    )?;
                }
                id
            };
            // Last observed receipt only. Replaying an older legacy cursor may
            // replace this pointer; it is neither materialization nor coverage.
            tx.execute(
                "INSERT INTO provider_shadow_receipts(scope,page_id) VALUES (?1,?2) ON CONFLICT(scope) DO UPDATE SET page_id=excluded.page_id",
                params![page.scope, page_id],
            )?;
            tx.commit()?;
            Ok(CapturedPageId(page_id))
        }).await
    }
}
