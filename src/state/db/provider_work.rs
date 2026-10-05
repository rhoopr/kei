//! Bounded current-generation admission; receipts do not own checkpoints.

use chrono::Utc;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::asset_writes::upsert_asset_row;
use super::membership::refresh_asset_album_groupings_tx;
use super::provider_inbox::{CapturedPageId, StoredPage, load_page};
use super::rows::encode_asset_date;
use super::{SqliteStateDb, account};
use crate::state::{AssetRecord, error::StateError};

pub(crate) const MAX_WORK_BYTES: u64 = 512 * 1024 * 1024;
const MAX_WORK_PLAN_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) struct WorkSource {
    pub(crate) page: StoredPage,
    pub(crate) ordinal: i64,
}

pub(crate) struct WorkPlan {
    pub(crate) source: WorkSource,
    pub(crate) scope: String,
    pub(crate) zone: serde_json::Value,
    pub(crate) config_hash: String,
    pub(crate) confirmation: Option<Vec<u8>>,
    pub(crate) master: Option<String>,
    pub(crate) records: Vec<AssetRecord>,
    /// Fixed internal diagnostic; never provider error text.
    pub(crate) reason: &'static str,
}

#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkAdmission {
    Admitted,
    Deferred,
}

struct WorkReceipt {
    state: String,
    reason: String,
    scope: String,
    source_hash: String,
    confirmation: Option<Vec<u8>>,
    master: Option<String>,
    charged_bytes: i64,
}

fn record_matches(
    conn: &rusqlite::Connection,
    record: &AssetRecord,
) -> Result<Option<bool>, StateError> {
    Ok(conn.query_row(
        "SELECT checksum=?4 AND size_bytes=?5 AND filename=?6 AND created_at IS ?7 AND added_at IS ?8 AND metadata_hash IS ?9 AND is_deleted=0 AND status IN ('pending','failed','downloaded') FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
        params![&record.library,&record.id,record.version_size.as_str(),&record.checksum,i64::try_from(record.size_bytes).map_err(|_invalid|StateError::ProviderWorkInvalid)?,&record.filename,
            encode_asset_date(record.created_at),record.added_at.map(encode_asset_date),record.metadata.metadata_hash.as_deref()],
        |row|row.get(0),
    ).optional()?)
}

impl SqliteStateDb {
    pub(crate) async fn next_work_source(
        &self,
        owner: account::AccountOwner,
        scope: String,
        config_hash: String,
    ) -> Result<Option<WorkSource>, StateError> {
        self.with_conn_mut("reading retained work source",move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let after:(i64,i64)=tx.query_row("SELECT page_id,ordinal FROM provider_work_scan WHERE scope=?1 AND config_hash=?2",params![scope,config_hash],|row|Ok((row.get(0)?,row.get(1)?))).optional()?.unwrap_or((0,-1));
            let id:Option<(i64,i64)>=tx.query_row(
                "SELECT c.page_id,c.ordinal FROM provider_catalog_records c JOIN provider_shadow_pages p ON p.id=c.page_id JOIN provider_catalog_pages projected ON projected.page_id=c.page_id AND projected.projector_version=1 AND projected.body_hash=p.body_hash WHERE p.scope=?1 AND c.kind='asset' AND c.deleted=0 AND (c.page_id,c.ordinal)>(?3,?4) AND NOT EXISTS(SELECT 1 FROM provider_work_receipts w WHERE w.page_id=c.page_id AND w.ordinal=c.ordinal AND w.config_hash=?2 AND w.state='admitted') ORDER BY c.page_id,c.ordinal LIMIT 1",
                params![scope,config_hash,after.0,after.1],|row|Ok((row.get(0)?,row.get(1)?)),
            ).optional()?;
            let out=if let Some((page,ordinal))=id {
                Some(WorkSource{page:load_page(&tx,CapturedPageId(page),crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES)?,ordinal})
            } else {
                // Fair scheduling only. Reset at EOF without declaring coverage.
                tx.execute("DELETE FROM provider_work_scan WHERE scope=?1 AND config_hash=?2",params![scope,config_hash])?;
                None
            };
            tx.commit()?;
            Ok(out)
        }).await
    }

    pub(crate) async fn project_provider_work(
        &self,
        owner: account::AccountOwner,
        plan: WorkPlan,
        capacity: u64,
    ) -> Result<WorkAdmission, StateError> {
        #[cfg(test)]
        let database = self.path.clone();
        self.with_conn_mut("admitting provider work",move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let current=load_page(&tx,plan.source.page.id,crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES)?;
            if current!=plan.source.page || current.page.scope!=plan.scope || plan.config_hash.len()!=64 {
                return Err(StateError::ProviderWorkInvalid);
            }
            let ordinal=usize::try_from(plan.source.ordinal).map_err(|_invalid|StateError::ProviderWorkInvalid)?;
            let identity=current.page.identities.get(ordinal).ok_or(StateError::ProviderWorkInvalid)?;
            let validated=crate::icloud::photos::catalog_observed_page(current.page.body.clone(),&plan.scope,&current.page.request_cursor).map_err(|_invalid|StateError::ProviderWorkInvalid)?;
            if validated!=current.page {return Err(StateError::ProviderWorkInvalid);}
            let indexed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_catalog_records c JOIN provider_catalog_pages p ON p.page_id=c.page_id WHERE c.page_id=?1 AND c.ordinal=?2 AND c.kind='asset' AND c.deleted=0 AND c.record_name=?3 AND p.projector_version=1 AND p.body_hash=?4)",params![current.id.0,plan.source.ordinal,identity.name,current.body_hash],|row|row.get(0))?;
            if !indexed || identity.record_type.as_deref()!=Some("CPLAsset") || identity.deleted {
                return Err(StateError::ProviderWorkInvalid);
            }
            let scope:serde_json::Value=serde_json::from_str(&plan.scope).map_err(|_invalid|StateError::ProviderWorkInvalid)?;
            let source_zone=scope.get("zone").ok_or(StateError::ProviderWorkInvalid)?;
            let zone=&plan.zone;
            if zone.get("zoneName")!=source_zone.get("zoneName") || zone.get("ownerRecordName")!=source_zone.get("ownerRecordName") {return Err(StateError::ProviderWorkInvalid);}
            let library=zone.get("zoneName").and_then(serde_json::Value::as_str).ok_or(StateError::ProviderWorkInvalid)?;
            if scope.get("database").and_then(serde_json::Value::as_str)!=Some("private") || zone.get("ownerRecordName").and_then(serde_json::Value::as_str)!=Some("_defaultOwner") {
                return Err(StateError::ProviderWorkInvalid);
            }
            let confirmation_hash=plan.confirmation.as_ref().map_or_else(String::new,|body|format!("{:x}",Sha256::digest(body)));
            let photo=match (&plan.confirmation,&plan.master) {
                (Some(body),Some(master)) if body.len()<=crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES => Some(crate::icloud::photos::current_asset(body,&identity.name,master,zone).map_err(|_invalid|StateError::ProviderWorkInvalid)?),
                (None,None) if plan.records.is_empty()=>None,
                _=>return Err(StateError::ProviderWorkInvalid),
            };
            if plan.records.len()>32 { return Err(StateError::ProviderWorkFull); }
            let mut versions=std::collections::HashSet::new();
            for record in &plan.records {
                let photo=photo.as_ref().ok_or(StateError::ProviderWorkInvalid)?;
                if record.library.as_ref()!=library || record.id.as_ref()!=identity.name || record.metadata.metadata_hash.as_deref()!=Some(record.metadata.compute_hash().as_str())
                    || record.created_at!=photo.created()
                    || record.added_at!=Some(photo.added_date())
                    || !versions.insert(record.version_size) || record.metadata.is_deleted {
                    return Err(StateError::ProviderWorkInvalid);
                }
                // The resource identity must be present in the retained current
                // response; logical rendition/RAW policy stays in the planner.
                if !photo.versions().iter().any(|(key,version)| {
                    let provider_key=crate::state::VersionSizeKey::from(*key);
                    let logical_matches=provider_key==record.version_size || matches!((provider_key,record.version_size),
                        (crate::state::VersionSizeKey::Original,crate::state::VersionSizeKey::Alternative)
                        | (crate::state::VersionSizeKey::Alternative,crate::state::VersionSizeKey::Original));
                    let metadata=photo.metadata_arc(provider_key);
                    logical_matches && version.checksum.as_ref()==record.checksum.as_ref() && version.size==record.size_bytes
                        && metadata.metadata_hash==record.metadata.metadata_hash && metadata.source==record.metadata.source
                        && metadata.provider_data==record.metadata.provider_data
                }) {
                    return Err(StateError::ProviderWorkInvalid);
                }
            }
            let charge=plan.confirmation.as_ref().map_or(0,|body|body.len() as u64)+plan.scope.len() as u64+plan.config_hash.len() as u64+plan.master.as_ref().map_or(0,|name|name.len() as u64)+identity.name.len() as u64+current.body_hash.len() as u64+plan.records.iter().map(|r|r.library.len() as u64+r.id.len() as u64+r.filename.len() as u64+r.checksum.len() as u64
                + [r.metadata.source.as_deref(),r.metadata.title.as_deref(),r.metadata.keywords.as_deref(),r.metadata.description.as_deref(),r.metadata.media_subtype.as_deref(),r.metadata.burst_id.as_deref(),r.metadata.provider_data.as_deref(),r.metadata.metadata_hash.as_deref()].iter().map(|value|value.map_or(0,|text|text.len() as u64)).sum::<u64>()+512).sum::<u64>()+256;
            if charge>MAX_WORK_PLAN_BYTES {return Err(StateError::ProviderWorkFull);}
            let existing:Option<WorkReceipt>=tx.query_row("SELECT state,reason,scope,source_hash,confirmation,master_record_name,charged_bytes FROM provider_work_receipts WHERE page_id=?1 AND ordinal=?2 AND config_hash=?3 AND confirmation_hash=?4",params![current.id.0,plan.source.ordinal,plan.config_hash,confirmation_hash],|r|Ok(WorkReceipt{state:r.get(0)?,reason:r.get(1)?,scope:r.get(2)?,source_hash:r.get(3)?,confirmation:r.get(4)?,master:r.get(5)?,charged_bytes:r.get(6)?})).optional()?;
            if let Some(receipt)=&existing {
                if receipt.scope!=plan.scope || receipt.source_hash!=current.body_hash || receipt.confirmation!=plan.confirmation || receipt.master!=plan.master {
                    return Err(StateError::ProviderWorkInvalid);
                }
                if receipt.state=="admitted" {
                    if !receipt.reason.is_empty() || receipt.charged_bytes!=i64::try_from(charge).map_err(|_invalid|StateError::ProviderWorkInvalid)? {return Err(StateError::ProviderWorkInvalid);}
                    // A receipt records admission, not ongoing queue contents.
                    // In particular, never resurrect an old generation here.
                    let actual:i64=tx.query_row("SELECT count(*) FROM provider_work_obligations WHERE page_id=?1 AND ordinal=?2 AND config_hash=?3 AND confirmation_hash=?4",params![current.id.0,plan.source.ordinal,plan.config_hash,confirmation_hash],|r|r.get(0))?;
                    if actual!=i64::try_from(plan.records.len()).map_err(|_invalid|StateError::ProviderWorkInvalid)? {return Err(StateError::ProviderWorkInvalid);}
                    for record in &plan.records {
                        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_work_obligations WHERE page_id=?1 AND ordinal=?2 AND config_hash=?3 AND confirmation_hash=?4 AND version_size=?5 AND library=?6 AND asset_id=?7 AND checksum=?8 AND size_bytes=?9 AND filename=?10 AND created_at IS ?11 AND added_at IS ?12 AND metadata_hash=?13)",params![current.id.0,plan.source.ordinal,plan.config_hash,confirmation_hash,record.version_size.as_str(),record.library,record.id,record.checksum,i64::try_from(record.size_bytes).map_err(|_invalid|StateError::ProviderWorkInvalid)?,record.filename,encode_asset_date(record.created_at),record.added_at.map(encode_asset_date),record.metadata.metadata_hash.as_deref()],|r|r.get(0))?;
                        if !valid {return Err(StateError::ProviderWorkInvalid);}
                    }
                    tx.commit()?;
                    return Ok(WorkAdmission::Admitted);
                }
            }
            let mut reason=plan.reason;
            if !plan.records.is_empty() {
                let master=plan.master.as_deref().ok_or(StateError::ProviderWorkInvalid)?;
                let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM unattributed_legacy WHERE library=?1 AND asset_id IN (?2,?3)) OR EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?3 AND id<>?2) OR EXISTS(SELECT 1 FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2 AND master_record_name<>?3) OR EXISTS(SELECT 1 FROM provider_work_obligations w JOIN provider_work_receipts r USING(page_id,ordinal,config_hash,confirmation_hash) WHERE w.library=?1 AND w.asset_id=?2 AND r.scope<>?4)",params![library,identity.name,master,plan.scope],|r|r.get(0))?;
                if blocked {reason="identity_conflict";}
                else if plan.records.iter().map(|record|record_matches(&tx,record)).collect::<Result<Vec<_>,_>>()?.contains(&Some(false)) {reason="queue_generation_conflict";}
            }
            let admitted=reason.is_empty() && !plan.records.is_empty();
            if let Some(receipt)=&existing {
                let obligations:i64=tx.query_row("SELECT count(*) FROM provider_work_obligations WHERE page_id=?1 AND ordinal=?2 AND config_hash=?3 AND confirmation_hash=?4",params![current.id.0,plan.source.ordinal,plan.config_hash,confirmation_hash],|r|r.get(0))?;
                if receipt.state!="deferred" || obligations!=0 {return Err(StateError::ProviderWorkInvalid);}
            }
            let previous_charge=existing.as_ref().map_or(Ok(0),|receipt|u64::try_from(receipt.charged_bytes).map_err(|_invalid|StateError::ProviderWorkInvalid))?;
            let used:i64=tx.query_row("SELECT COALESCE(SUM(charged_bytes),0) FROM provider_work_receipts",[],|r|r.get(0))?;
            if charge.saturating_sub(previous_charge)>capacity.saturating_sub(u64::try_from(used).map_err(|_invalid|StateError::ProviderWorkInvalid)?) {return Err(StateError::ProviderWorkFull);}
            if admitted {
                let now=Utc::now().timestamp();
                for record in &plan.records {
                    if record_matches(&tx,record)?.is_none() {
                        upsert_asset_row(&tx,record,now)?;
                        refresh_asset_album_groupings_tx(&tx,library,&identity.name,None)?;
                        tx.execute("UPDATE assets SET metadata_write_failed_at=COALESCE(metadata_write_failed_at,?3) WHERE library=?1 AND id=?2 AND EXISTS(SELECT 1 FROM asset_albums WHERE library=?1 AND asset_id=?2)",params![library,identity.name,now])?;
                    }
                    tx.execute("INSERT INTO provider_work_obligations(page_id,ordinal,config_hash,confirmation_hash,version_size,library,asset_id,checksum,size_bytes,filename,created_at,added_at,metadata_hash) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",params![current.id.0,plan.source.ordinal,plan.config_hash,confirmation_hash,record.version_size.as_str(),record.library,record.id,record.checksum,i64::try_from(record.size_bytes).map_err(|_invalid|StateError::ProviderWorkInvalid)?,record.filename,encode_asset_date(record.created_at),record.added_at.map(encode_asset_date),record.metadata.metadata_hash.as_deref()])?;
                }
                tx.execute("INSERT INTO asset_master_mappings(library,asset_record_name,master_record_name,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(library,asset_record_name) DO NOTHING",params![library,identity.name,plan.master,now])?;
            }
            tx.execute("INSERT INTO provider_work_scan(scope,config_hash,page_id,ordinal) VALUES (?1,?2,?3,?4) ON CONFLICT(scope,config_hash) DO UPDATE SET page_id=excluded.page_id,ordinal=excluded.ordinal",params![plan.scope,plan.config_hash,current.id.0,plan.source.ordinal])?;
            #[cfg(test)]
            pause_before_receipt(&database)?;
            // Last write: queue/mapping/debt/scan and receipt commit together.
            tx.execute("INSERT INTO provider_work_receipts(page_id,ordinal,config_hash,confirmation_hash,confirmation,state,reason,scope,source_hash,master_record_name,charged_bytes) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) ON CONFLICT(page_id,ordinal,config_hash,confirmation_hash) DO UPDATE SET state=excluded.state,reason=excluded.reason,charged_bytes=excluded.charged_bytes",params![current.id.0,plan.source.ordinal,plan.config_hash,confirmation_hash,plan.confirmation,if admitted {"admitted"} else {"deferred"},if admitted {""} else if reason.is_empty() {"no_selected_tasks"} else {reason},plan.scope,current.body_hash,plan.master,i64::try_from(charge).map_err(|_invalid|StateError::ProviderWorkFull)?])?;
            tx.commit()?;
            Ok(if admitted {WorkAdmission::Admitted} else {WorkAdmission::Deferred})
        }).await
    }
}

// Additional paths keep their independent publication receipts. A canonical
// downloaded row does not complete a failed or prepared publication elsewhere.
const UNFINISHED_GENERATION: &str = "a.checksum=w.checksum AND a.size_bytes=w.size_bytes AND a.metadata_hash IS w.metadata_hash AND (a.status IN ('pending','failed') OR a.metadata_write_failed_at IS NOT NULL OR a.capture_repair_metadata_hash IS NOT NULL OR a.capture_repair_output_checksum IS NOT NULL OR a.capture_repair_output_size IS NOT NULL OR EXISTS(SELECT 1 FROM asset_metadata_paths p WHERE p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.provider_checksum=a.checksum AND (p.metadata_write_failed_at IS NOT NULL OR p.capture_repair_metadata_hash IS NOT NULL OR p.capture_repair_output_checksum IS NOT NULL OR p.capture_repair_output_size IS NOT NULL)))";

// Keep the existing writer contract when this additive owner has no evidence.
fn has_projected_obligation(
    conn: &rusqlite::Connection,
    library: &str,
    asset: &str,
    version: Option<&str>,
) -> Result<bool, StateError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provider_work_obligations WHERE library=?1 AND asset_id=?2 AND (?3 IS NULL OR version_size=?3))",
        params![library, asset, version], |row| row.get(0),
    )?)
}

/// Historical admissions of completed generations cannot block ordinary updates.
pub(super) fn guard_projected_generation(
    conn: &rusqlite::Connection,
    record: &AssetRecord,
) -> Result<(), StateError> {
    if !has_projected_obligation(
        conn,
        &record.library,
        &record.id,
        Some(record.version_size.as_str()),
    )? {
        return Ok(());
    }
    // A collision-resolved leaf is path-planner evidence, not a content
    // generation. Existing reservations and publication still own that path.
    let computed_hash = record.metadata.compute_hash();
    let metadata_hash = record
        .metadata
        .metadata_hash
        .as_deref()
        .unwrap_or(&computed_hash);
    let conflict:bool=conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM provider_work_obligations w JOIN assets a ON a.library=w.library AND a.id=w.asset_id AND a.version_size=w.version_size WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND {UNFINISHED_GENERATION} AND (w.checksum<>?4 OR w.size_bytes IS NOT ?5 OR w.created_at IS NOT ?6 OR w.added_at IS NOT ?7 OR w.metadata_hash IS NOT ?8))"),
        params![record.library,record.id,record.version_size.as_str(),record.checksum,i64::try_from(record.size_bytes).ok(),encode_asset_date(record.created_at),record.added_at.map(encode_asset_date),metadata_hash],|r|r.get(0),
    )?;
    if conflict {
        Err(StateError::ProviderWorkConflict)
    } else {
        Ok(())
    }
}

pub(super) fn guard_projected_mapping(
    conn: &rusqlite::Connection,
    library: &str,
    child: &str,
    master: &str,
) -> Result<(), StateError> {
    if !has_projected_obligation(conn, library, child, None)? {
        return Ok(());
    }
    let conflict:bool=conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM provider_work_obligations w JOIN provider_work_receipts r USING(page_id,ordinal,config_hash,confirmation_hash) JOIN assets a ON a.library=w.library AND a.id=w.asset_id AND a.version_size=w.version_size WHERE a.library=?1 AND a.id=?2 AND {UNFINISHED_GENERATION} AND r.master_record_name<>?3)"),params![library,child,master],|r|r.get(0))?;
    if conflict {
        Err(StateError::ProviderWorkConflict)
    } else {
        Ok(())
    }
}

pub(super) fn guard_projected_metadata(
    conn: &rusqlite::Connection,
    library: &str,
    asset: &str,
    version: &str,
    metadata: &crate::state::AssetMetadata,
    created: f64,
    added: Option<f64>,
) -> Result<(), StateError> {
    if !has_projected_obligation(conn, library, asset, Some(version))? {
        return Ok(());
    }
    let conflict:bool=conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM provider_work_obligations w JOIN assets a ON a.library=w.library AND a.id=w.asset_id AND a.version_size=w.version_size WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND {UNFINISHED_GENERATION} AND (w.metadata_hash IS NOT ?4 OR w.created_at IS NOT ?5 OR w.added_at IS NOT ?6))"),params![library,asset,version,metadata.metadata_hash.as_deref(),created,added],|r|r.get(0))?;
    if conflict {
        Err(StateError::ProviderWorkConflict)
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn pause_before_receipt(database: &std::path::Path) -> Result<(), StateError> {
    let Some(marker) = std::env::var_os("KEI_TEST_WORK_COMMIT_PAUSE") else {
        return Ok(());
    };
    std::fs::write(&marker, database.as_os_str().as_encoded_bytes()).map_err(|source| {
        StateError::TempPath {
            path: marker.clone().into(),
            source,
        }
    })?;
    // Only the isolated process-death child sets this marker. The parent kills
    // that child after independently observing the real uncommitted writes.
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(StateError::ProviderWorkInvalid)
}
