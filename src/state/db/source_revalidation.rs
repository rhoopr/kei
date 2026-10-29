//! Additive, baseline-bound source verification debt. It never owns media.
use super::{SqliteStateDb, account};
use crate::state::error::StateError;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

pub(crate) const DDL: &str = r"
CREATE TABLE IF NOT EXISTS source_revalidation_rounds (
 id TEXT PRIMARY KEY, library TEXT NOT NULL, scope TEXT NOT NULL,
 enum_hash TEXT NOT NULL, boundary TEXT NOT NULL, high_id TEXT NOT NULL,
 after_id TEXT NOT NULL, phase TEXT NOT NULL CHECK(phase IN ('marking','observing','active','abandoned'))
);
CREATE TABLE IF NOT EXISTS source_revalidation_debt (
 round TEXT NOT NULL REFERENCES source_revalidation_rounds(id),
 id TEXT NOT NULL, next_retry_at INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(round,id)
);
CREATE INDEX IF NOT EXISTS source_revalidation_due ON source_revalidation_debt(next_retry_at,round,id);
";

#[derive(Clone, Debug)]
pub(crate) struct SourceCandidate {
    pub(crate) round: String,
    pub(crate) library: String,
    pub(crate) id: String,
    pub(crate) master: String,
    pub(crate) child: Option<String>,
    pub(crate) owned_child: bool,
    pub(crate) snapshot: String,
}

fn snapshot(
    conn: &rusqlite::Connection,
    round: &str,
    id: &str,
    master: &str,
) -> Result<String, StateError> {
    Ok(conn.query_row(
        "SELECT json_array(
          (SELECT json_group_array(json_array(id,version_size,checksum,filename,size_bytes,status,local_path,local_checksum,download_checksum,metadata_hash,is_deleted,deleted_at,last_seen_at,downloaded_at))
           FROM (SELECT * FROM assets WHERE library=r.library AND (id=?2 OR id=?3 OR id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=r.library AND master_record_name=?3)) ORDER BY id,version_size)),
          (SELECT json_group_array(json_array(asset_record_name,master_record_name,updated_at))
           FROM (SELECT * FROM asset_master_mappings WHERE library=r.library AND (asset_record_name=?2 OR master_record_name=?3 OR asset_record_name IN (SELECT asset_record_name FROM legacy_master_state_owners WHERE library=r.library AND master_record_name=?3)) ORDER BY asset_record_name)),
          (SELECT json_group_array(json_array(asset_id,evidence_version,original_evidence,files,active_generation))
           FROM (SELECT * FROM unattributed_legacy WHERE library=r.library AND (asset_id=?2 OR asset_id=?3 OR asset_id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=r.library AND master_record_name=?3)) ORDER BY asset_id)),
          (SELECT json_group_array(json_array(id,version_size,state,reason,checked_at)) FROM (SELECT * FROM asset_verifications WHERE library=r.library AND (id=?2 OR id=?3 OR id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=r.library AND master_record_name=?3)) ORDER BY id,version_size)),
          (SELECT json_array(asset_record_name,claimed_at) FROM legacy_master_state_owners WHERE library=r.library AND master_record_name=?3),
          (SELECT next_retry_at FROM source_revalidation_debt WHERE round=r.id AND id=?2),
          r.scope,r.enum_hash,r.boundary,r.phase,
          (SELECT value FROM metadata WHERE key='sync_token:v27:'||r.library),
          (SELECT value FROM metadata WHERE key='observed_selection:v27:'||r.library),
          (SELECT value FROM metadata WHERE key='enum_config_hash'),
          (SELECT value FROM metadata WHERE key='pending_enum_config_hash'),
          (SELECT value FROM metadata WHERE key='config_hash'),
          (SELECT value FROM metadata WHERE key='pending_download_config_hash')
         ) FROM source_revalidation_rounds r WHERE r.id=?1",
        params![round,id,master], |row| row.get(0),
    )?)
}

pub(crate) fn has_due(conn: &rusqlite::Connection, library: &str) -> Result<bool, StateError> {
    let scopes: Vec<(String, String)> = conn.prepare("SELECT DISTINCT r.scope,r.enum_hash FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library=?1 AND r.phase='active' AND r.enum_hash=COALESCE((SELECT value FROM metadata WHERE key='pending_enum_config_hash'),(SELECT value FROM metadata WHERE key='enum_config_hash')) AND d.next_retry_at<=?2")?
        .query_map(params![library, chrono::Utc::now().timestamp()], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
    for (scope, policy) in scopes {
        if super::provider_generations::observation::revalidation_scope_current(
            conn, library, &scope, &policy,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Count active-round obligations separately from existing asset verification.
/// Deferred obligations remain eligible; unfinished rounds and stale scopes remain retained.
/// Superseded abandoned rounds remain durable but are not future obligations.
pub(crate) fn summary_counts(conn: &rusqlite::Connection) -> Result<(u64, u64), StateError> {
    let groups: Vec<(String, String, String, String, i64)> = conn.prepare(
        "SELECT r.library,r.scope,r.enum_hash,r.phase,COUNT(*) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.phase != 'abandoned' GROUP BY r.library,r.scope,r.enum_hash,r.phase"
    )?.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?.collect::<Result<_,_>>()?;
    let effective: Option<String> = conn.query_row(
        "SELECT COALESCE((SELECT value FROM metadata WHERE key='pending_enum_config_hash'),(SELECT value FROM metadata WHERE key='enum_config_hash'))", [], |row| row.get(0)
    )?;
    let mut eligible = 0u64;
    let mut retained = 0u64;
    for (library, scope, policy, phase, count) in groups {
        let count = u64::try_from(count).map_err(|_error| StateError::ProviderSelectionInvalid)?;
        if phase == "active"
            && effective.as_deref() == Some(policy.as_str())
            && super::provider_generations::observation::revalidation_scope_current(
                conn, &library, &scope, &policy,
            )?
        {
            eligible = eligible.saturating_add(count);
        } else {
            retained = retained.saturating_add(count);
        }
    }
    Ok((eligible, retained))
}

impl SqliteStateDb {
    /// Finish bounded candidate marking before the corresponding inventory begins.
    pub(crate) async fn seed_source_revalidation(
        &self,
        owner: account::AccountOwner,
        library: String,
        scope: String,
        enum_hash: String,
        boundary: String,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), StateError> {
        let initial_owner = owner.clone();
        let round = self.with_conn_mut("starting source revalidation", move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&initial_owner)?;
            let existing:Option<String>=tx.query_row(
                "SELECT id FROM source_revalidation_rounds WHERE library=?1 AND scope=?2 AND enum_hash=?3 AND boundary=?4 AND phase='marking' ORDER BY id LIMIT 1",
                params![library,scope,enum_hash,boundary], |r| r.get(0)).optional()?;
            let id=if let Some(id)=existing {id} else {
                tx.execute("UPDATE source_revalidation_rounds SET phase='abandoned' WHERE library=?1 AND scope=?2 AND enum_hash=?3 AND boundary=?4 AND phase='observing'", params![library,scope,enum_hash,boundary])?;
                let id=uuid::Uuid::new_v4().to_string();
                let high:Option<String>=tx.query_row(
                    "SELECT MAX(id) FROM assets WHERE library=?1 AND (status='downloaded' OR downloaded_at IS NOT NULL) AND is_deleted=0",
                    [&library],|r|r.get(0))?;
                tx.execute("INSERT INTO source_revalidation_rounds VALUES(?1,?2,?3,?4,?5,?6,'','marking')",
                    params![id,library,scope,enum_hash,boundary,high.unwrap_or_default()])?;
                id
            };
            tx.commit()?;Ok(id)
        }).await?;
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            let id = round.clone();
            let owner = owner.clone();
            let done=self.with_conn_mut("marking source revalidation page",move |conn| {
                let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                account::validate_authenticated(&tx, &owner)?;
                let (library,high,after,phase):(String,String,String,String)=tx.query_row(
                    "SELECT library,high_id,after_id,phase FROM source_revalidation_rounds WHERE id=?1",
                    [&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                if phase!="marking" {tx.commit()?;return Ok(true)}
                let rows:Vec<String>=tx.prepare("SELECT DISTINCT id FROM assets WHERE library=?1 AND id>?2 AND id<=?3 AND (status='downloaded' OR downloaded_at IS NOT NULL) AND is_deleted=0 ORDER BY id LIMIT 64")?
                    .query_map(params![library,after,high],|r|r.get(0))?.collect::<Result<_,_>>()?;
                for child in &rows {
                    tx.execute("INSERT OR IGNORE INTO source_revalidation_debt(round,id) VALUES(?1,?2)",params![id,child])?;
                }
                if let Some(last)=rows.last() {
                    tx.execute("UPDATE source_revalidation_rounds SET after_id=?2 WHERE id=?1",params![id,last])?;
                } else {
                    tx.execute("UPDATE source_revalidation_rounds SET phase='observing' WHERE id=?1",[&id])?;
                }
                let done=rows.is_empty(); tx.commit()?;Ok(done)
            }).await?;
            if done {
                return Ok(());
            }
        }
    }

    pub(crate) async fn source_revalidation_seen(
        &self,
        owner: account::AccountOwner,
        library: String,
        scope: String,
        id: String,
    ) -> Result<(), StateError> {
        self.with_conn_mut("settling observed source", move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            tx.execute("DELETE FROM source_revalidation_debt WHERE id=?1 AND round IN
                (SELECT id FROM source_revalidation_rounds WHERE library=?2 AND scope=?3 AND phase='observing')",
                params![id,library,scope])?;
            tx.commit()?;Ok(())
        }).await
    }

    pub(crate) async fn due_source_revalidation(
        &self,
        owner: account::AccountOwner,
        library: String,
        scope: String,
    ) -> Result<Vec<SourceCandidate>, StateError> {
        self.with_conn("reading due source revalidation",move |conn| {
            account::validate_authenticated(conn,&owner)?;
            let pairs:Vec<(String,String,String)>=conn.prepare(
                "SELECT d.round,d.id,r.enum_hash FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round
                 WHERE r.library=?1 AND r.scope=?2 AND r.phase='active' AND r.enum_hash=COALESCE((SELECT value FROM metadata WHERE key='pending_enum_config_hash'),(SELECT value FROM metadata WHERE key='enum_config_hash')) AND d.next_retry_at<=?3 ORDER BY d.next_retry_at,d.round,d.id LIMIT 64")?
                .query_map(params![library,scope,chrono::Utc::now().timestamp()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
            let mut output=Vec::new();
            for (round,id,enum_hash) in pairs {
                if !super::provider_generations::observation::revalidation_scope_current(conn, &library, &scope, &enum_hash)? { continue; }
                let mapped:Option<String>=conn.query_row("SELECT master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2",params![library,id],|r|r.get(0)).optional()?;
                let master=mapped.clone().unwrap_or_else(||id.clone());
                let (child,owned_child)=if mapped.is_some() {
                    (Some(id.clone()), false)
                } else {
                    let owner:Option<(String,Option<String>)>=conn.query_row(
                        "SELECT o.asset_record_name,m.master_record_name FROM legacy_master_state_owners o LEFT JOIN asset_master_mappings m ON m.library=o.library AND m.asset_record_name=o.asset_record_name WHERE o.library=?1 AND o.master_record_name=?2",
                        params![library,master],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                    match owner {
                        Some((child,Some(mapped_master))) if mapped_master==master => (Some(child),true),
                        _ => (None,false),
                    }
                };
                output.push(SourceCandidate{snapshot:snapshot(conn,&round,&id,&master)?,round,library:library.clone(),id,master,child,owned_child});
            }
            Ok(output)
        }).await
    }

    pub(crate) async fn settle_source_revalidation(
        &self,
        owner: account::AccountOwner,
        candidate: SourceCandidate,
        deleted: Option<(bool, Option<chrono::DateTime<chrono::Utc>>)>,
        live: bool,
    ) -> Result<bool, StateError> {
        self.with_conn_mut("settling confirmed source revalidation", move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE d.round=?1 AND d.id=?2 AND r.phase='active')",params![candidate.round,candidate.id],|r|r.get(0))?;
            if !exists { tx.commit()?; return Ok(false); }
            if snapshot(&tx,&candidate.round,&candidate.id,&candidate.master)? != candidate.snapshot {
                tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
                tx.commit()?;return Ok(false)
            }
            let deleted = deleted.filter(|(family, _)| *family || candidate.id != candidate.master || candidate.owned_child);
            if let Some((family,date))=deleted {
                let guard = if family {
                    super::asset_writes::ensure_master_family_has_no_prepared_capture_repair(&tx,&candidate.library,&candidate.master,"source_revalidation")
                } else {
                    super::asset_writes::ensure_asset_has_no_prepared_capture_repair(&tx,&candidate.library,&candidate.id,"source_revalidation")
                };
                match guard {
                    Ok(()) => (),
                    Err(StateError::Invariant { operation: "source_revalidation", .. }) => {
                        tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
                        tx.commit()?; return Ok(false);
                    },
                    Err(error) => return Err(error),
                }
                if family {
                    tx.execute("UPDATE assets SET is_deleted=1,deleted_at=COALESCE(?3,deleted_at) WHERE library=?1 AND NOT EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=assets.library AND p.asset_id=assets.id) AND (id=?2 OR id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2))",params![candidate.library,candidate.master,date.map(|v|v.timestamp())])?;
                } else {
                    tx.execute("UPDATE assets SET is_deleted=1,deleted_at=COALESCE(?3,deleted_at) WHERE library=?1 AND id=?2 AND NOT EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=assets.library AND p.asset_id=assets.id)",params![candidate.library,candidate.id,date.map(|v|v.timestamp())])?;
                }
                let remaining:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND is_deleted=0)",params![candidate.library,candidate.id],|r|r.get(0))?;
                if remaining {
                    tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
                    tx.commit()?;return Ok(false)
                }
            }
            let live = live && (candidate.id != candidate.master || candidate.owned_child);
            if deleted.is_some() || live {
                tx.execute("DELETE FROM source_revalidation_debt WHERE round=?1 AND id=?2",params![candidate.round,candidate.id])?;
            } else {
                tx.execute("UPDATE source_revalidation_debt SET next_retry_at=?3 WHERE round=?1 AND id=?2",params![candidate.round,candidate.id,chrono::Utc::now().timestamp()+3600])?;
            }
            tx.commit()?;Ok(true)
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::VersionSizeKey;
    use serde_json::json;

    fn owner() -> account::AccountOwner {
        account::AccountOwner::authenticated(
            "source-revalidation@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic"}})).unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn blocked_or_changed_candidate_is_deferred_without_losing_other_debt() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_owned(&dir.path().join("db"), &owner())
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("item")
            .library("PrimarySync")
            .version_size(VersionSizeKey::Original)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "item",
            "original",
            &dir.path().join("item"),
            "synthetic",
            None,
        )
        .await
        .unwrap();
        let mut candidate = {
            let conn = db.acquire_lock("synthetic source debt").unwrap();
            conn.execute("INSERT INTO source_revalidation_rounds VALUES('round','PrimarySync','scope','policy','boundary','item','item','active')", []).unwrap();
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES('round','item')",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO asset_verifications VALUES('PrimarySync','item','original','unknown','other debt',1)", []).unwrap();
            let snapshot = snapshot(&conn, "round", "item", "item").unwrap();
            conn.execute("UPDATE assets SET last_error='local reconcile' WHERE library='PrimarySync' AND id='item'", []).unwrap();
            // status is part of the snapshot, unlike the informational last_error.
            conn.execute(
                "UPDATE assets SET status='failed' WHERE library='PrimarySync' AND id='item'",
                [],
            )
            .unwrap();
            SourceCandidate {
                round: "round".into(),
                library: "PrimarySync".into(),
                id: "item".into(),
                master: "item".into(),
                child: None,
                owned_child: false,
                snapshot,
            }
        };
        assert!(
            !db.settle_source_revalidation(owner(), candidate.clone(), Some((true, None)), false)
                .await
                .unwrap()
        );
        {
            let conn = db.acquire_lock("verify deferred stale candidate").unwrap();
            let (when, deleted):(i64,i64) = conn.query_row("SELECT d.next_retry_at,a.is_deleted FROM source_revalidation_debt d JOIN assets a ON a.library='PrimarySync' AND a.id=d.id", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert!(when > chrono::Utc::now().timestamp());
            assert_eq!(deleted, 0);
            conn.execute("UPDATE source_revalidation_debt SET next_retry_at=0", [])
                .unwrap();
            conn.execute("INSERT INTO unattributed_legacy VALUES('PrimarySync','item',1,'evidence','[]',1,NULL)", []).unwrap();
            candidate.snapshot = snapshot(&conn, "round", "item", "item").unwrap();
        }
        assert!(
            !db.settle_source_revalidation(owner(), candidate, Some((true, None)), false)
                .await
                .unwrap()
        );
        let conn = db
            .acquire_lock("verify deferred protected candidate")
            .unwrap();
        let (when, deleted, other):(i64,i64,i64) = conn.query_row("SELECT d.next_retry_at,a.is_deleted,(SELECT COUNT(*) FROM asset_verifications WHERE reason='other debt') FROM source_revalidation_debt d JOIN assets a ON a.library='PrimarySync' AND a.id=d.id", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert!(when > chrono::Utc::now().timestamp());
        assert_eq!((deleted, other), (0, 1));
    }
}
