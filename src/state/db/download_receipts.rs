//! Compact completed downloads. These rows own receipts, never retries or cursors.
use super::provider_selection::SelectionPath;
use crate::state::error::StateError;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const DDL: &str = r"
CREATE TABLE IF NOT EXISTS download_receipts (
 id TEXT PRIMARY KEY, account_key TEXT NOT NULL, provider_key TEXT NOT NULL,
 scope TEXT NOT NULL, library TEXT NOT NULL, child TEXT NOT NULL, master TEXT NOT NULL,
 state_id TEXT NOT NULL, origin_pass TEXT NOT NULL, version_size TEXT NOT NULL, provider_checksum TEXT NOT NULL,
 provider_size INTEGER NOT NULL CHECK(provider_size>=0), native_path TEXT NOT NULL,
 source_checksum TEXT NOT NULL, local_checksum TEXT NOT NULL, proof_hash TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS download_receipts_identity ON download_receipts(scope,child,master,version_size,provider_checksum,provider_size);
CREATE INDEX IF NOT EXISTS download_receipts_library ON download_receipts(library,scope);
";

// Preserve the old source-backed rows byte-for-byte while permitting a typed
// local-trust alternative in the same compact completion store.
pub(crate) const LOCAL_TRUST_MIGRATION: &str = r"
ALTER TABLE download_receipts RENAME TO download_receipts_v40;
CREATE TABLE download_receipts (
 id TEXT PRIMARY KEY, account_key TEXT NOT NULL, provider_key TEXT NOT NULL,
 scope TEXT NOT NULL, library TEXT NOT NULL, child TEXT NOT NULL, master TEXT NOT NULL,
 state_id TEXT NOT NULL, origin_pass TEXT NOT NULL, version_size TEXT NOT NULL, provider_checksum TEXT NOT NULL,
 provider_size INTEGER NOT NULL CHECK(provider_size>=0), native_path TEXT NOT NULL,
 source_checksum TEXT, local_checksum TEXT NOT NULL, proof_hash TEXT NOT NULL,
 trust_receipt TEXT, selected_metadata_hash TEXT, generation_id TEXT,
 CHECK ((source_checksum IS NOT NULL AND trust_receipt IS NULL AND selected_metadata_hash IS NULL AND generation_id IS NULL) OR
        (source_checksum IS NULL AND trust_receipt IS NOT NULL AND selected_metadata_hash IS NOT NULL AND generation_id IS NOT NULL))
) WITHOUT ROWID;
INSERT INTO download_receipts(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash)
 SELECT id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash FROM download_receipts_v40;
DROP TABLE download_receipts_v40;
CREATE INDEX download_receipts_identity ON download_receipts(scope,child,master,version_size,provider_checksum,provider_size);
CREATE INDEX download_receipts_library ON download_receipts(library,scope);
";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DownloadReceipt {
    pub(crate) account_key: String,
    pub(crate) provider_key: String,
    pub(crate) scope: String,
    pub(crate) library: String,
    pub(crate) child: String,
    pub(crate) master: String,
    pub(crate) state_id: String,
    pub(crate) origin_pass: String,
    pub(crate) version_size: String,
    pub(crate) provider_checksum: String,
    pub(crate) provider_size: u64,
    pub(crate) native_path: SelectionPath,
    pub(crate) source_checksum: String,
    pub(crate) local_checksum: String,
}

/// Compact selected completion based solely on explicit local acceptance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LocalTrustCompletion {
    pub(super) version: u8,
    pub(super) account_key: String,
    pub(super) provider_key: String,
    pub(super) scope: String,
    pub(super) library: String,
    pub(super) child: String,
    pub(super) master: String,
    pub(super) state_id: String,
    pub(super) origin_pass: String,
    pub(super) version_size: String,
    pub(super) provider_checksum: String,
    pub(super) provider_size: u64,
    pub(super) native_path: SelectionPath,
    pub(super) local_checksum: String,
    pub(super) trust_receipt: String,
    pub(super) selected_metadata_hash: String,
    pub(super) generation_id: String,
}

impl LocalTrustCompletion {
    fn validate(&self, conn: &Connection) -> Result<(), StateError> {
        let owner: (String, String) = conn.query_row(
            "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let scope: serde_json::Value =
            serde_json::from_str(&self.scope).map_err(|_error| invalid())?;
        let accepted =
            super::trusted_legacy::receipt_file(conn, &self.trust_receipt)?.ok_or_else(invalid)?;
        if self.version != 1
            || owner != (self.account_key.clone(), self.provider_key.clone())
            || !matches!(
                scope.get("database").and_then(serde_json::Value::as_str),
                Some("private" | "shared")
            )
            || scope
                .pointer("/zone/zoneName")
                .and_then(serde_json::Value::as_str)
                != Some(self.library.as_str())
            || scope
                .pointer("/zone/ownerRecordName")
                .and_then(serde_json::Value::as_str)
                .is_none_or(str::is_empty)
            || self.library != accepted.library
            || self.child != accepted.child
            || self.state_id != accepted.child
            || self.master != accepted.master
            || self.version_size != accepted.rendition
            || self.native_path.to_path() != accepted.path
            || self.local_checksum != accepted.local_sha256
            || self.origin_pass.is_empty()
            || self.provider_checksum.is_empty()
            || self.selected_metadata_hash.is_empty()
            || self.generation_id.is_empty()
            || self.provider_size > i64::MAX as u64
            || !self.native_path.to_path().is_absolute()
            || SelectionPath::from_path(&self.native_path.to_path()) != self.native_path
            || crate::state::VersionSizeKey::from_str(&self.version_size).is_none()
            || self.local_checksum.len() != 64
            || !self.local_checksum.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn id(&self) -> Result<String, StateError> {
        Ok(format!("local_trust:v1:{}", hash(self)?))
    }

    pub(super) fn save(&self, conn: &Connection) -> Result<(), StateError> {
        self.validate(conn)?;
        let id = self.id()?;
        let proof = hash(self)?;
        conn.execute("INSERT OR IGNORE INTO download_receipts(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,trust_receipt,selected_metadata_hash,generation_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,NULL,?14,?15,?16,?17,?18)",
            params![id,self.account_key,self.provider_key,self.scope,self.library,self.child,self.master,self.state_id,self.origin_pass,self.version_size,self.provider_checksum,self.provider_size as i64,self.native_path.key()?,self.local_checksum,proof,self.trust_receipt,self.selected_metadata_hash,self.generation_id])?;
        if load_local_trust(conn, &id)?.as_ref() != Some(self) {
            return Err(invalid());
        }
        Ok(())
    }
}

pub(super) fn load_local_trust(
    conn: &Connection,
    id: &str,
) -> Result<Option<LocalTrustCompletion>, StateError> {
    let row: Option<(LocalTrustCompletion, String)> = conn.query_row(
        "SELECT account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,local_checksum,trust_receipt,proof_hash,selected_metadata_hash,generation_id FROM download_receipts WHERE id=?1 AND source_checksum IS NULL AND trust_receipt IS NOT NULL",
        [id], |r| {
            let path: String = r.get(11)?;
            Ok((LocalTrustCompletion { version: 1, account_key: r.get(0)?, provider_key:r.get(1)?, scope:r.get(2)?, library:r.get(3)?, child:r.get(4)?, master:r.get(5)?, state_id:r.get(6)?, origin_pass:r.get(7)?, version_size:r.get(8)?, provider_checksum:r.get(9)?, provider_size:u64::try_from(r.get::<_,i64>(10)?).map_err(|_error| rusqlite::Error::InvalidQuery)?, native_path:serde_json::from_str(&path).map_err(|_error| rusqlite::Error::InvalidQuery)?, local_checksum:r.get(12)?, trust_receipt:r.get(13)?, selected_metadata_hash:r.get(15)?, generation_id:r.get(16)? }, r.get(14)?))
        }).optional()?;
    let Some((completion, proof)) = row else {
        return Ok(None);
    };
    completion.validate(conn)?;
    if completion.id()? != id || hash(&completion)? != proof {
        return Err(invalid());
    }
    Ok(Some(completion))
}

/// A compact local receipt is only a path hint until its immutable acceptance
/// still matches current account and legacy dependencies.
pub(super) fn local_trust_is_current(
    conn: &Connection,
    completion: &LocalTrustCompletion,
) -> Result<bool, StateError> {
    Ok(super::trusted_legacy::accepted_file(
        conn,
        &completion.library,
        &completion.child,
        &completion.version_size,
    )?
    .is_some_and(|(key, file)| {
        key == completion.trust_receipt
            && file.path == completion.native_path.to_path()
            && file.local_sha256 == completion.local_checksum
    }))
}

fn invalid() -> StateError {
    StateError::ProviderSelectionInvalid
}
fn hash(value: &impl Serialize) -> Result<String, StateError> {
    let bytes = serde_json::to_vec(value).map_err(|_invalid| invalid())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
impl DownloadReceipt {
    // A receipt may satisfy several selected passes at the same physical
    // path. This is a unique set of fulfilled obligations, never a run log.
    fn fulfilled_passes(&self) -> Result<Vec<String>, StateError> {
        if !self.origin_pass.starts_with('[') {
            if self.origin_pass.trim().is_empty() {
                return Err(invalid());
            }
            return Ok(vec![self.origin_pass.clone()]);
        }
        let passes: Vec<String> =
            serde_json::from_str(&self.origin_pass).map_err(|_invalid| invalid())?;
        if passes.is_empty()
            || passes.iter().any(|pass| pass.trim().is_empty())
            || !passes
                .windows(2)
                .all(|pair| matches!(pair, [left, right] if left < right))
        {
            return Err(invalid());
        }
        Ok(passes)
    }
    pub(crate) fn fulfills_pass(&self, key: &str) -> bool {
        self.fulfilled_passes()
            .is_ok_and(|passes| passes.iter().any(|pass| pass == key))
    }
    fn validate(&self, conn: &Connection) -> Result<(), StateError> {
        self.fulfilled_passes()?;
        let owner: (String, String) = conn.query_row(
            "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let scope: serde_json::Value =
            serde_json::from_str(&self.scope).map_err(|_invalid| invalid())?;
        if owner != (self.account_key.clone(), self.provider_key.clone())
            || !matches!(
                scope.get("database").and_then(serde_json::Value::as_str),
                Some("private" | "shared")
            )
            || scope
                .pointer("/zone/ownerRecordName")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|owner| owner.trim().is_empty())
            || self.origin_pass.trim().is_empty()
            || self.library.trim().is_empty()
            || scope
                .pointer("/zone/zoneName")
                .and_then(serde_json::Value::as_str)
                != Some(self.library.as_str())
            || self.child.is_empty()
            || self.master.is_empty()
            || self.state_id.is_empty()
            || self.provider_checksum.is_empty()
            || self.provider_size > i64::MAX as u64
            || crate::state::VersionSizeKey::from_str(&self.version_size).is_none()
            || [&self.source_checksum, &self.local_checksum]
                .iter()
                .any(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
            || !self.native_path.to_path().is_absolute()
            || SelectionPath::from_path(&self.native_path.to_path()) != self.native_path
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub(crate) fn id(&self) -> Result<String, StateError> {
        hash(&(
            &self.account_key,
            &self.provider_key,
            &self.scope,
            &self.child,
            &self.master,
            &self.state_id,
            &self.version_size,
            &self.provider_checksum,
            self.provider_size,
            &self.native_path,
        ))
    }
    pub(super) fn save(&self, conn: &Connection) -> Result<String, StateError> {
        self.validate(conn)?;
        let id = self.id()?;
        if let Some(previous) = load(conn, &id)? {
            let mut same = self.clone();
            same.origin_pass = previous.origin_pass.clone();
            if previous != same {
                return Err(StateError::ProviderWorkConflict);
            }
            let mut passes = previous.fulfilled_passes()?;
            let old_passes = passes.clone();
            passes.extend(self.fulfilled_passes()?);
            passes.sort();
            passes.dedup();
            if passes != old_passes {
                let mut merged = previous;
                merged.origin_pass =
                    serde_json::to_string(&passes).map_err(|_invalid| invalid())?;
                conn.execute(
                    "UPDATE download_receipts SET origin_pass=?2,proof_hash=?3 WHERE id=?1",
                    params![id, merged.origin_pass, hash(&merged)?],
                )?;
            }
            return Ok(id);
        }
        conn.execute("INSERT INTO download_receipts(id,account_key,provider_key,scope,library,child,master,state_id,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,origin_pass) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![id,self.account_key,self.provider_key,self.scope,self.library,self.child,self.master,self.state_id,self.version_size,self.provider_checksum,self.provider_size as i64,self.native_path.key()?,self.source_checksum,self.local_checksum,hash(self)?,self.origin_pass])?;
        Ok(id)
    }
}
/// Called only after an operation's prepared proof was validated in this transaction.
pub(super) fn save_prepared_rewrite(
    conn: &Connection,
    completed: &DownloadReceipt,
    input_checksum: &str,
) -> Result<(), StateError> {
    if conn.is_autocommit() {
        return Err(invalid());
    }
    let id = completed.id()?;
    if let Some(previous) = load(conn, &id)? {
        let mut expected = completed.clone();
        expected.origin_pass = previous.origin_pass.clone();
        expected.local_checksum = previous.local_checksum.clone();
        if expected != previous
            || (previous.local_checksum != input_checksum
                && previous.local_checksum != completed.local_checksum)
        {
            return Err(StateError::ProviderWorkConflict);
        }
        super::provider_generations::advance_completed_receipt(conn, &previous, completed)?;
    }
    completed.save(conn)?;
    Ok(())
}

pub(super) fn replace_validated_local(
    conn: &Connection,
    previous: &DownloadReceipt,
    local: &str,
) -> Result<(), StateError> {
    let mut updated = previous.clone();
    updated.local_checksum = local.to_owned();
    conn.execute(
        "UPDATE download_receipts SET local_checksum=?2,proof_hash=?3 WHERE id=?1",
        params![updated.id()?, updated.local_checksum, hash(&updated)?],
    )?;
    Ok(())
}

pub(super) fn load(conn: &Connection, id: &str) -> Result<Option<DownloadReceipt>, StateError> {
    let saved:Option<(DownloadReceipt,String)>=conn.query_row("SELECT account_key,provider_key,scope,library,child,master,state_id,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,origin_pass FROM download_receipts WHERE id=?1",[id],|r| {
        let path:String=r.get(10)?;
        let path=serde_json::from_str(&path).map_err(|_invalid|rusqlite::Error::InvalidQuery)?;
        Ok((DownloadReceipt{account_key:r.get(0)?,provider_key:r.get(1)?,scope:r.get(2)?,library:r.get(3)?,child:r.get(4)?,master:r.get(5)?,state_id:r.get(6)?,origin_pass:r.get(14)?,version_size:r.get(7)?,provider_checksum:r.get(8)?,provider_size:u64::try_from(r.get::<_,i64>(9)?).map_err(|_invalid|rusqlite::Error::InvalidQuery)?,native_path:path,source_checksum:r.get(11)?,local_checksum:r.get(12)?},r.get(13)?))
    }).optional()?;
    let Some((receipt, proof)) = saved else {
        return Ok(None);
    };
    receipt.validate(conn)?;
    if receipt.id()? != id || hash(&receipt)? != proof {
        return Err(invalid());
    }
    Ok(Some(receipt))
}
pub(super) fn matching(
    conn: &Connection,
    scope: &str,
    child: &str,
    master: &str,
    version: &str,
    checksum: &str,
    size: u64,
) -> Result<Vec<DownloadReceipt>, StateError> {
    let ids:Vec<String>=conn.prepare("SELECT id FROM download_receipts WHERE scope=?1 AND child=?2 AND master=?3 AND version_size=?4 AND provider_checksum=?5 AND provider_size=?6 AND source_checksum IS NOT NULL ORDER BY native_path")?
        .query_map(params![scope,child,master,version,checksum,i64::try_from(size).map_err(|_invalid|invalid())?],|r|r.get(0))?.collect::<Result<_,_>>()?;
    ids.into_iter()
        .map(|id| load(conn, &id)?.ok_or_else(invalid))
        .collect()
}

/// Historical receipts classify an occupied old path, never current content.
pub(super) fn historical(
    conn: &Connection,
    scope: &str,
    child: &str,
    master: &str,
    version: &str,
) -> Result<Vec<DownloadReceipt>, StateError> {
    let ids: Vec<String> = conn.prepare("SELECT id FROM download_receipts WHERE scope=?1 AND child=?2 AND master=?3 AND version_size=?4 AND source_checksum IS NOT NULL ORDER BY native_path")?
        .query_map(params![scope, child, master, version], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    ids.into_iter()
        .map(|id| load(conn, &id)?.ok_or_else(invalid))
        .collect()
}

pub(super) fn matching_local(
    conn: &Connection,
    scope: &str,
    child: &str,
    path: &SelectionPath,
    local: &str,
) -> Result<Vec<DownloadReceipt>, StateError> {
    let ids:Vec<String>=conn.prepare("SELECT id FROM download_receipts WHERE scope=?1 AND child=?2 AND native_path=?3 AND local_checksum=?4 AND source_checksum IS NOT NULL ORDER BY id")?
        .query_map(params![scope,child,path.key()?,local],|r|r.get(0))?.collect::<Result<_,_>>()?;
    ids.into_iter()
        .map(|id| load(conn, &id)?.ok_or_else(invalid))
        .collect()
}

#[cfg(test)]
mod local_trust_migration_tests {
    use super::{DDL, LOCAL_TRUST_MIGRATION};
    use rusqlite::Connection;

    #[test]
    fn local_trust_migration_preserves_source_receipt_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute("INSERT INTO download_receipts VALUES('id','account','provider','scope','library','child','master','state','pass','original','checksum',9,'path','source','local','proof')", []).unwrap();
        let before: String = conn.query_row("SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts", [], |r| r.get(0)).unwrap();
        conn.execute_batch(LOCAL_TRUST_MIGRATION).unwrap();
        let after: String = conn.query_row("SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts", [], |r| r.get(0)).unwrap();
        assert_eq!(before, after);
        assert!(
            conn.execute(
                "UPDATE download_receipts SET source_checksum=NULL WHERE id='id'",
                []
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE download_receipts SET trust_receipt='trust' WHERE id='id'",
                []
            )
            .is_err()
        );
    }
}
