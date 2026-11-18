//! Atomic source indexing, separate from queue consumption and checkpoints.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::provider_inbox::{CapturedPageId, StoredPage, load_page};
use super::{SqliteStateDb, account};
use crate::state::error::StateError;
use crate::support::{StorageComponent, StorageOutcome};

pub(crate) const PROJECTOR_VERSION: i64 = 1;

pub(crate) struct CatalogRecord {
    pub(crate) kind: &'static str,
    pub(crate) references: Vec<CatalogReference>,
    pub(crate) debt: Vec<CatalogDebt>,
}

pub(crate) struct CatalogReference {
    pub(crate) path: String,
    pub(crate) target: String,
    pub(crate) zone_name: Option<String>,
    pub(crate) zone_owner: Option<String>,
}

pub(crate) struct CatalogDebt {
    pub(crate) path: String,
    pub(crate) reason: &'static str,
}

pub(crate) struct CatalogPlan {
    pub(crate) source: StoredPage,
    pub(crate) records: Vec<CatalogRecord>,
    pub(crate) charged_bytes: u64,
}

/// Release only if every source record has an independent durable owner.
/// Keep compact facts and dependency hashes in place for generation replay.
fn finish_consumed(conn: &rusqlite::Connection, plan: &CatalogPlan) -> Result<(), StateError> {
    let source = &plan.source;
    let id = source.id.0;
    if plan.records.is_empty()
        || plan.records.iter().any(|r| {
            !matches!(r.kind, "asset" | "master")
                || r.debt.iter().any(|d| {
                    d.reason != "unresolved_reference" || d.path != "/fields/masterRef/value"
                })
        })
    {
        return Ok(());
    }
    let pinned: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_work_receipts WHERE page_id=?1 UNION ALL SELECT 1 FROM provider_work_retries WHERE page_id=?1 UNION ALL SELECT 1 FROM provider_selection_sources WHERE page_id=?1 UNION ALL SELECT 1 FROM provider_transport_aliases WHERE page_id=?1 OR canonical_page_id=?1 UNION ALL SELECT 1 FROM provider_shadow_receipts WHERE page_id=?1)",[id],|r|r.get(0))?;
    if pinned {
        return Ok(());
    }
    let records = crate::icloud::photos::change_records(&source.page.body)
        .map_err(|_invalid| StateError::ProviderCatalogInvalid)?;
    for (record, identity) in records.iter().zip(&source.page.identities) {
        if identity.deleted {
            // A state ID alone is ambiguous across libraries and families.
            // Accept only a simple hard tombstone with a known private owner,
            // whose exact state and all mapped renditions have been deleted.
            if !crate::icloud::photos::plain_primary_tombstone(&source.page.scope, record) {
                return Ok(());
            }
            let applied:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE library='PrimarySync' AND id=?1) AND NOT EXISTS(SELECT 1 FROM assets WHERE library='PrimarySync' AND id=?1 AND is_deleted<>1) AND NOT EXISTS(SELECT 1 FROM asset_master_mappings WHERE library='PrimarySync' AND (master_record_name=?1 OR asset_record_name=?1))",[&identity.name],|r|r.get(0))?;
            if !applied {
                return Ok(());
            }
        } else if !super::provider_generations::published_change_matches(
            conn,
            &source.page.scope,
            &identity.name,
            record,
        )? {
            return Ok(());
        }
    }
    let binding = super::provider_inbox::release_binding(conn, id)?;
    conn.execute(
        "INSERT INTO provider_finished_changes(page_id,receipt) VALUES(?1,?2)",
        params![id, binding],
    )?;
    conn.execute(
        "UPDATE provider_shadow_pages SET body=X'' WHERE id=?1",
        [id],
    )?;
    Ok(())
}

impl SqliteStateDb {
    /// Only revisit projected pages with a possible downstream handoff. An
    /// account may have other unfinished pages ahead of a newly completed one.
    pub(crate) async fn next_handoff_candidate(
        &self,
        owner: account::AccountOwner,
        after: i64,
        max: usize,
    ) -> Result<Option<StoredPage>, StateError> {
        self.with_conn("rechecking completed inbox work",move|conn| {
            let tx=conn.unchecked_transaction()?;
            account::validate(&tx,&owner)?;
            let id:Option<i64>=tx.query_row("SELECT p.id FROM provider_shadow_pages p JOIN provider_catalog_pages c ON p.id=c.page_id AND p.body_hash=c.body_hash AND c.projector_version=?2 WHERE p.id>?1 AND c.record_count>0 AND NOT EXISTS(SELECT 1 FROM provider_finished_changes f WHERE f.page_id=p.id) AND NOT EXISTS(SELECT 1 FROM provider_catalog_records r WHERE r.page_id=p.id AND (r.kind NOT IN ('asset','master') OR (r.deleted=0 AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w WHERE (w.child=r.record_name OR w.master=r.record_name) AND w.current_publication=1)) OR (r.deleted=1 AND NOT EXISTS(SELECT 1 FROM assets a WHERE a.library=json_extract(p.scope,'$.zone.zoneName') AND a.id=r.record_name AND a.is_deleted=1)))) ORDER BY p.id LIMIT 1",params![after,PROJECTOR_VERSION],|r|r.get(0)).optional()?;
            id.map(|id|load_page(&tx,CapturedPageId(id),max)).transpose()
        }).await
    }
    pub(crate) async fn validate_empty_poll(
        &self,
        owner: account::AccountOwner,
        page: super::provider_inbox::ObservedPage,
        realm: std::sync::Arc<str>,
    ) -> Result<(), StateError> {
        self.with_conn("validating empty provider poll", move |conn| {
            account::validate(conn, &owner)?;
            crate::icloud::photos::validate_empty_poll(&page, &realm)
                .map_err(|_invalid| StateError::ProviderCatalogInvalid)
        })
        .await
    }
    /// Empty observations have no records to pair, schedule or include in a
    /// generation basis. Revalidate the original, not just the receipt count.
    /// The durable cursor is deliberately outside this transaction.
    pub(crate) async fn reclaim_empty_catalog_pages(
        &self,
        owner: account::AccountOwner,
        max_body_bytes: usize,
    ) -> Result<(), StateError> {
        self.with_conn_mut("releasing completed provider polling pages", move |conn| {
            let mut after = 0;
            loop {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                account::validate(&tx, &owner)?;
                let id: Option<i64> = tx.query_row(
                    "SELECT c.page_id FROM provider_catalog_pages c JOIN provider_shadow_pages p ON p.id=c.page_id
                     WHERE c.page_id>?1 AND c.projector_version=?2 AND c.body_hash=p.body_hash AND c.record_count=0 AND c.reference_count=0 AND c.debt_count=0
                     AND NOT EXISTS(SELECT 1 FROM provider_shadow_receipts WHERE page_id=p.id)
                     AND NOT EXISTS(SELECT 1 FROM provider_transport_aliases WHERE page_id=p.id OR canonical_page_id=p.id)
                     AND NOT EXISTS(SELECT 1 FROM provider_transport_canonical WHERE page_id=p.id)
                     ORDER BY c.page_id LIMIT 1", rusqlite::params![after, PROJECTOR_VERSION], |row| row.get(0),
                ).optional()?;
                let Some(id) = id else { break };
                let source = load_page(&tx, CapturedPageId(id), max_body_bytes)?;
                let plan = crate::icloud::photos::plan_retained(&source)
                    .map_err(|_invalid| StateError::ProviderCatalogInvalid)?;
                if !plan.records.is_empty() || !source.page.identities.is_empty() {
                    return Err(StateError::ProviderCatalogInvalid);
                }
                let charge = i64::try_from(plan.charged_bytes).map_err(|_invalid| StateError::ProviderCatalogInvalid)?;
                let removed = tx.execute("DELETE FROM provider_catalog_pages WHERE page_id=?1 AND charged_bytes=?2 AND NOT EXISTS(SELECT 1 FROM provider_catalog_records WHERE page_id=?1)", params![id,charge])?;
                if removed != 1 { return Err(StateError::ProviderCatalogInvalid); }
                tx.execute("DELETE FROM provider_shadow_pages WHERE id=?1", [id])?;
                tx.commit()?;
                after = id;
            }
            Ok(())
        }).await
    }

    pub(crate) async fn next_catalog_source(
        &self,
        owner: account::AccountOwner,
        max_body_bytes: usize,
    ) -> Result<Option<StoredPage>, StateError> {
        self.with_conn("reading pending catalog source", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate(&tx, &owner)?;
            let id: Option<i64> = tx.query_row(
                "SELECT p.id FROM provider_shadow_pages p LEFT JOIN provider_catalog_pages c ON c.page_id=p.id WHERE c.page_id IS NULL OR c.projector_version<>?1 ORDER BY p.id LIMIT 1",
                [PROJECTOR_VERSION], |row| row.get(0),
            ).optional()?;
            let source = id.map(|id| load_page(&tx,CapturedPageId(id),max_body_bytes)).transpose()?;
            tx.commit()?;
            Ok(source)
        }).await
    }

    #[cfg(test)]
    pub(crate) async fn catalog_source(
        &self,
        owner: account::AccountOwner,
        id: CapturedPageId,
        max_body_bytes: usize,
    ) -> Result<StoredPage, StateError> {
        self.with_conn("reading catalog source", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate(&tx, &owner)?;
            let source = load_page(&tx, id, max_body_bytes)?;
            tx.commit()?;
            Ok(source)
        })
        .await
    }

    pub(crate) async fn active_catalog_source(
        &self,
        owner: account::AccountOwner,
        id: CapturedPageId,
        max: usize,
    ) -> Result<Option<StoredPage>, StateError> {
        self.with_conn("reading active provider payload", move |conn| {
            let tx = conn.unchecked_transaction()?;
            account::validate(&tx, &owner)?;
            if super::provider_inbox::released(&tx, id.0)? {
                return Ok(None);
            }
            load_page(&tx, id, max).map(Some)
        })
        .await
    }

    pub(crate) async fn project_catalog_page(
        &self,
        owner: account::AccountOwner,
        plan: CatalogPlan,
        capacity: u64,
    ) -> Result<(), StateError> {
        self.with_conn_mut("projecting catalog source page", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate(&tx,&owner)?;
            if super::provider_inbox::released(&tx,plan.source.id.0)? {return Ok(())}
            let current = load_page(&tx,plan.source.id,plan.source.page.body.len())?;
            if current != plan.source || plan.records.len() != current.page.identities.len() {
                return Err(StateError::ProviderCatalogInvalid);
            }
            let page_id = current.id.0;
            let records = i64::try_from(plan.records.len()).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            let references = i64::try_from(plan.records.iter().map(|r|r.references.len()).sum::<usize>()).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            let debt = i64::try_from(plan.records.iter().map(|r|r.debt.len()).sum::<usize>()).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            let charged = i64::try_from(plan.charged_bytes).map_err(|_out_of_range|StateError::ProviderCatalogFull)?;
            let expected = (PROJECTOR_VERSION,current.observation_hash.clone(),records,references,debt,charged);
            let existing: Option<(i64,String,i64,i64,i64,i64)> = tx.query_row(
                "SELECT projector_version,body_hash,record_count,reference_count,debt_count,charged_bytes FROM provider_catalog_pages WHERE page_id=?1",
                [page_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
            ).optional()?;
            if let Some(receipt) = existing {
                if receipt != expected { return Err(StateError::ProviderCatalogInvalid); }
                for (table,count) in [("provider_catalog_records",records),("provider_catalog_references",references),("provider_catalog_debt",debt)] {
                    let actual: i64 = tx.query_row(&format!("SELECT count(*) FROM {table} WHERE page_id=?1"),[page_id],|row|row.get(0))?;
                    if actual != count { return Err(StateError::ProviderCatalogInvalid); }
                }
                for (ordinal,(record,identity)) in plan.records.iter().zip(&current.page.identities).enumerate() {
                    let ordinal = i64::try_from(ordinal).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
                    let valid: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM provider_catalog_records WHERE page_id=?1 AND ordinal=?2 AND record_name=?3 AND record_type IS ?4 AND deleted=?5 AND kind=?6)",
                        params![page_id,ordinal,identity.name,identity.record_type,identity.deleted,record.kind], |row| row.get(0),
                    )?;
                    if !valid { return Err(StateError::ProviderCatalogInvalid); }
                    for reference in &record.references {
                        let valid: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM provider_catalog_references WHERE page_id=?1 AND ordinal=?2 AND field_path=?3 AND target_record_name=?4 AND target_zone_name IS ?5 AND target_zone_owner IS ?6)",
                            params![page_id,ordinal,reference.path,reference.target,reference.zone_name,reference.zone_owner],|row|row.get(0),
                        )?;
                        if !valid { return Err(StateError::ProviderCatalogInvalid); }
                    }
                    for evidence in &record.debt {
                        let valid: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM provider_catalog_debt WHERE page_id=?1 AND ordinal=?2 AND field_path=?3 AND reason=?4)",
                            params![page_id,ordinal,evidence.path,evidence.reason],|row|row.get(0),
                        )?;
                        if !valid { return Err(StateError::ProviderCatalogInvalid); }
                    }
                }
                finish_consumed(&tx,&plan)?;
                tx.commit()?;
                crate::support::record_storage(
                    StorageComponent::Catalogue,
                    StorageOutcome::Exact,
                    None,
                );
                return Ok(());
            }
            let used: i64 = tx.query_row("SELECT COALESCE(SUM(charged_bytes),0) FROM provider_catalog_pages",[],|row|row.get(0))?;
            let used = u64::try_from(used).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            if plan.charged_bytes > capacity.saturating_sub(used) {
                crate::support::record_storage(
                    StorageComponent::Catalogue,
                    StorageOutcome::CapacityRejected,
                    Some((used, capacity)),
                );
                return Err(StateError::ProviderCatalogFull);
            }
            for (ordinal,(record,identity)) in plan.records.iter().zip(&current.page.identities).enumerate() {
                let ordinal = i64::try_from(ordinal).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
                tx.execute(
                    "INSERT INTO provider_catalog_records(page_id,ordinal,record_name,record_type,deleted,kind) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![page_id,ordinal,identity.name,identity.record_type,identity.deleted,record.kind],
                )?;
                for reference in &record.references {
                    tx.execute(
                        "INSERT INTO provider_catalog_references(page_id,ordinal,field_path,target_record_name,target_zone_name,target_zone_owner) VALUES (?1,?2,?3,?4,?5,?6)",
                        params![page_id,ordinal,reference.path,reference.target,reference.zone_name,reference.zone_owner],
                    )?;
                }
                for evidence in &record.debt {
                    tx.execute(
                        "INSERT INTO provider_catalog_debt(page_id,ordinal,field_path,reason) VALUES (?1,?2,?3,?4)",
                        params![page_id,ordinal,evidence.path,evidence.reason],
                    )?;
                }
            }
            // This means source facts were indexed, including unresolved debt.
            // It is never download/rewrite completion or provider coverage.
            tx.execute(
                "INSERT INTO provider_catalog_pages(page_id,projector_version,body_hash,record_count,reference_count,debt_count,charged_bytes) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![page_id,expected.0,expected.1,records,references,debt,charged],
            )?;
            finish_consumed(&tx,&plan)?;
            tx.commit()?;
            crate::support::record_storage(
                StorageComponent::Catalogue,
                StorageOutcome::New,
                Some((used, capacity)),
            );
            Ok(())
        }).await
    }
}
