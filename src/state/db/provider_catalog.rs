//! Atomic source indexing, separate from queue consumption and checkpoints.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::provider_inbox::{CapturedPageId, StoredPage, load_page};
use super::{SqliteStateDb, account};
use crate::state::error::StateError;

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

impl SqliteStateDb {
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

    pub(crate) async fn project_catalog_page(
        &self,
        owner: account::AccountOwner,
        plan: CatalogPlan,
        capacity: u64,
    ) -> Result<(), StateError> {
        self.with_conn_mut("projecting catalog source page", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate(&tx,&owner)?;
            let current = load_page(&tx,plan.source.id,plan.source.page.body.len())?;
            if current != plan.source || plan.records.len() != current.page.identities.len() {
                return Err(StateError::ProviderCatalogInvalid);
            }
            let page_id = current.id.0;
            let records = i64::try_from(plan.records.len()).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            let references = i64::try_from(plan.records.iter().map(|r|r.references.len()).sum::<usize>()).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            let debt = i64::try_from(plan.records.iter().map(|r|r.debt.len()).sum::<usize>()).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            let charged = i64::try_from(plan.charged_bytes).map_err(|_out_of_range|StateError::ProviderCatalogFull)?;
            let expected = (PROJECTOR_VERSION,current.body_hash.clone(),records,references,debt,charged);
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
                tx.commit()?;
                return Ok(());
            }
            let used: i64 = tx.query_row("SELECT COALESCE(SUM(charged_bytes),0) FROM provider_catalog_pages",[],|row|row.get(0))?;
            let used = u64::try_from(used).map_err(|_out_of_range|StateError::ProviderCatalogInvalid)?;
            if plan.charged_bytes > capacity.saturating_sub(used) {
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
            tx.commit()?;
            Ok(())
        }).await
    }
}
