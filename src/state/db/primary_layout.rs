//! Preservation-backed primary layout journals. Historical receipts are immutable.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::SqliteStateDb;
use super::provider_selection::SelectionPath;
use crate::state::{AssetRecord, error::StateError};

pub(crate) const DDL: &str = r"
CREATE TABLE IF NOT EXISTS primary_layout_bindings (
 family TEXT PRIMARY KEY, library TEXT NOT NULL, source_library TEXT NOT NULL, child TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation > 0), decision TEXT NOT NULL,
 policy TEXT NOT NULL, binding BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS primary_layout_operations (
 operation TEXT PRIMARY KEY, family TEXT NOT NULL, library TEXT NOT NULL, source_library TEXT NOT NULL,
 prior_generation INTEGER NOT NULL, decision TEXT NOT NULL, policy TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('planned','prepared','preserved','publishing','committed','conflict','cancelled')),
 header BLOB NOT NULL, conflict TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS primary_layout_one_pending ON primary_layout_operations(family) WHERE phase NOT IN ('committed','cancelled');
CREATE TABLE IF NOT EXISTS primary_layout_members (
 operation TEXT NOT NULL REFERENCES primary_layout_operations(operation),
 member INTEGER NOT NULL, evidence BLOB NOT NULL, PRIMARY KEY(operation,member)
);
CREATE TABLE IF NOT EXISTS primary_layout_claims (
 path_key TEXT PRIMARY KEY, family TEXT NOT NULL, operation TEXT,
 library TEXT NOT NULL, child TEXT NOT NULL, version TEXT NOT NULL,
 provider_checksum TEXT NOT NULL, local_checksum TEXT,
 native_path BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS primary_layout_preserved (
 operation TEXT NOT NULL, member INTEGER NOT NULL, native_path BLOB NOT NULL,
 media_checksum TEXT NOT NULL, sidecar_checksum TEXT, evidence BLOB NOT NULL,
 PRIMARY KEY(operation,member)
);
CREATE TABLE IF NOT EXISTS primary_layout_superseded_paths (
 library TEXT NOT NULL, child TEXT NOT NULL, version TEXT NOT NULL,
 provider_checksum TEXT NOT NULL, native_path BLOB NOT NULL, compat_path TEXT,
 PRIMARY KEY(library,child,version,provider_checksum,native_path)
);
CREATE INDEX IF NOT EXISTS primary_layout_superseded_compat ON primary_layout_superseded_paths(library,child,version,provider_checksum,compat_path);
CREATE INDEX IF NOT EXISTS primary_layout_binding_source ON primary_layout_bindings(source_library);
CREATE INDEX IF NOT EXISTS primary_layout_binding_owner ON primary_layout_bindings(library,child);
CREATE INDEX IF NOT EXISTS primary_layout_claim_owner ON primary_layout_claims(library,child);
CREATE INDEX IF NOT EXISTS primary_layout_pending_library ON primary_layout_operations(source_library,phase);
";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayoutFingerprint {
    pub(crate) size: u64,
    pub(crate) sha256: [u8; 32],
    pub(crate) identity: crate::fs_util::FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayoutFile {
    pub(crate) path: SelectionPath,
    pub(crate) version: String,
    pub(crate) provider_checksum: String,
    pub(crate) fingerprint: LayoutFingerprint,
    pub(crate) source_checksum: Option<String>,
    pub(crate) sidecar: Option<LayoutFingerprint>,
    pub(crate) archive_original: bool,
    pub(crate) metadata_decision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayoutBinding {
    pub(crate) family: String,
    pub(crate) library: String,
    pub(crate) source_library: String,
    pub(crate) child: String,
    pub(crate) generation: i64,
    pub(crate) decision: String,
    pub(crate) policy: String,
    pub(crate) qualifier: String,
    pub(crate) files: Vec<LayoutFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayoutMember {
    pub(crate) record: Option<AssetRecord>,
    pub(crate) destination: Option<SelectionPath>,
    pub(crate) path_key: Option<String>,
    pub(crate) stage: Option<SelectionPath>,
    pub(crate) old: Option<LayoutFile>,
    pub(crate) archive_source: Option<LayoutFile>,
    pub(crate) preserved_path: Option<SelectionPath>,
    pub(crate) retirement: Option<SelectionPath>,
    pub(crate) prepared: Option<LayoutFile>,
    pub(crate) preserved: Option<LayoutFile>,
    pub(crate) installed: Option<LayoutFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayoutOperation {
    pub(crate) operation: String,
    pub(crate) family: String,
    pub(crate) library: String,
    pub(crate) source_library: String,
    pub(crate) child: String,
    pub(crate) asset_record_name: String,
    pub(crate) pass: String,
    pub(crate) metadata_flags: u8,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) refresh_metadata: bool,
    pub(crate) root: SelectionPath,
    pub(crate) generation: i64,
    pub(crate) decision: String,
    pub(crate) policy: String,
    pub(crate) qualifier: String,
    pub(crate) phase: String,
    pub(crate) members: Vec<LayoutMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PreservedManifestFile {
    pub(crate) operation: String,
    pub(crate) family: String,
    pub(crate) previous_generation: i64,
    pub(crate) phase: String,
    pub(crate) native_path: SelectionPath,
    pub(crate) provider_checksum: String,
    pub(crate) local_checksum: String,
    pub(crate) source_checksum: Option<String>,
    pub(crate) sidecar_checksum: Option<String>,
    pub(crate) size_bytes: u64,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

fn invalid(detail: impl Into<String>) -> StateError {
    StateError::Invariant {
        operation: "primary_layout",
        detail: detail.into(),
    }
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, StateError> {
    let bytes = serde_json::to_vec(value).map_err(|_error| invalid("invalid layout evidence"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("layout member exceeds evidence budget"));
    }
    Ok(bytes)
}
pub(super) fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StateError> {
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("stored layout evidence exceeds budget"));
    }
    serde_json::from_slice(bytes).map_err(|_error| invalid("invalid stored layout evidence"))
}

pub(crate) fn path_key(path: &std::path::Path) -> Result<String, StateError> {
    let path =
        crate::fs_util::absolute_lexical(path).map_err(|_error| invalid("invalid layout path"))?;
    // Match the existing case and AM/PM collision equivalence without lossy native paths.
    let path = if let Some(text) = path.to_str() {
        std::path::PathBuf::from(crate::download::paths::normalize_ampm(text).to_lowercase())
    } else {
        path
    };
    SelectionPath::from_path(&path).key()
}

pub(super) fn guard_slot(
    conn: &Connection,
    library: &str,
    child: &str,
    version: &str,
    checksum: &str,
    path: &std::path::Path,
) -> Result<(), StateError> {
    let slot: Option<(String,String,String,String,Option<String>)> = conn.query_row(
        "SELECT library,child,version,provider_checksum,operation FROM primary_layout_claims WHERE path_key=?1",
        [path_key(path)?], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    if let Some((owner, asset, rendition, content, pending)) = slot
        && (pending.is_some()
            || owner != library
            || asset != child
            || rendition != version
            || content != checksum)
    {
        return Err(invalid(
            "path is fenced by another primary layout generation",
        ));
    }
    Ok(())
}

/// Finalization must preserve the managed generation's exact local receipt.
pub(super) fn guard_downloaded_checksum(
    conn: &Connection,
    path: &std::path::Path,
    checksum: &str,
) -> Result<(), StateError> {
    let expected: Option<Option<String>> = conn
        .query_row(
            "SELECT local_checksum FROM primary_layout_claims WHERE path_key=?1",
            [path_key(path)?],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(expected) = expected
        && expected.as_deref() != Some(checksum)
    {
        return Err(invalid(
            "downloaded receipt differs from managed primary bytes",
        ));
    }
    Ok(())
}

pub(super) fn guard_writer(conn: &Connection, path: &std::path::Path) -> Result<(), StateError> {
    let managed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?1)",
        [path_key(path)?],
        |r| r.get(0),
    )?;
    if managed {
        return Err(invalid(
            "managed primary slot requires its layout publication owner",
        ));
    }
    Ok(())
}

impl SqliteStateDb {
    pub(crate) async fn primary_layout_binding(
        &self,
        family: String,
    ) -> Result<Option<LayoutBinding>, StateError> {
        self.with_conn("primary_layout_binding", move |conn| {
            let bytes: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT binding FROM primary_layout_bindings WHERE family=?1",
                    [&family],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(bytes)=bytes else {return Ok(None)};
            let binding:LayoutBinding=decode(&bytes)?;
            let matches:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_bindings WHERE family=?1 AND library=?2 AND source_library=?3 AND child=?4 AND generation=?5 AND decision=?6 AND policy=?7)",params![family,binding.library,binding.source_library,binding.child,binding.generation,binding.decision,binding.policy],|row|row.get(0))?;
            if !matches || binding.family!=family || binding.files.len()>16 {return Err(invalid("stored layout binding identity is inconsistent"));}
            Ok(Some(binding))
        })
        .await
    }
    pub(crate) async fn primary_layout_operations(
        &self,
        library: String,
    ) -> Result<Vec<LayoutOperation>, StateError> {
        self.with_conn("primary_layout_operations", move |conn| {
            let mut stmt=conn.prepare("SELECT header FROM primary_layout_operations WHERE source_library=?1 AND phase NOT IN ('committed','cancelled') ORDER BY operation")?;
            let headers=stmt.query_map([&library],|r|r.get::<_,Vec<u8>>(0))?.collect::<Result<Vec<_>,_>>()?;
            let mut result=Vec::new();
            for header in headers {
                let mut op: LayoutOperation=decode(&header)?;
                let matches:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_operations WHERE operation=?1 AND family=?2 AND library=?3 AND source_library=?4 AND prior_generation=?5 AND decision=?6 AND policy=?7 AND phase=?8)",params![op.operation,op.family,op.library,library,op.generation,op.decision,op.policy,op.phase],|row|row.get(0))?;
                if !matches || op.source_library!=library || !op.members.is_empty() {return Err(invalid("stored layout operation identity is inconsistent"));}
                let mut stmt=conn.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member LIMIT 17")?;
                op.members=stmt.query_map([&op.operation],|r|r.get::<_,Vec<u8>>(0))?.map(|row|decode(&row?)).collect::<Result<Vec<_>,StateError>>()?;
                if op.members.is_empty() || op.members.len()>16 {return Err(invalid("stored layout membership exceeds budget"));}
                result.push(op);
            }
            Ok(result)
        }).await
    }
    pub(crate) async fn begin_primary_layout(&self, op: LayoutOperation) -> Result<(), StateError> {
        self.with_conn("begin_primary_layout", move |conn| {
            if op.members.is_empty() || op.members.len()>16 || op.phase!="planned" || op.generation<0 || !matches!(op.policy.as_str(),"primary"|"suffix") { return Err(invalid("invalid layout plan")); }
            let mut destinations=std::collections::HashSet::new();
            for member in &op.members {
                if let Some(record)=&member.record
                    && (record.library.as_ref()!=op.library || record.id.as_ref()!=op.child || member.destination.is_none() || member.stage.is_none()) {
                    return Err(invalid("layout member has inconsistent rendition scope"));
                }
                if let Some(destination)=&member.destination {
                    let key=path_key(&destination.to_path())?;
                    if member.path_key.as_deref()!=Some(key.as_str()) || !destinations.insert(key) {return Err(invalid("layout destinations are inconsistent or duplicated"));}
                }
            }
            let tx=conn.unchecked_transaction()?;
            let generation:i64=tx.query_row("SELECT generation FROM primary_layout_bindings WHERE family=?1",[&op.family],|r|r.get(0)).optional()?.unwrap_or(0);
            if generation!=op.generation { return Err(invalid("stale primary layout plan")); }
            let mut header=op.clone();header.members.clear();
            tx.execute("INSERT INTO primary_layout_operations(operation,family,library,source_library,prior_generation,decision,policy,phase,header) VALUES(?1,?2,?3,?4,?5,?6,?7,'planned',?8)",params![op.operation,op.family,op.library,op.source_library,op.generation,op.decision,op.policy,encode(&header)?])?;
            for (index,member) in op.members.iter().enumerate() {
                tx.execute("INSERT INTO primary_layout_members VALUES(?1,?2,?3)",params![op.operation,u32::try_from(index).map_err(|_error|invalid("layout member index exceeds budget"))?,encode(member)?])?;
                if let (Some(destination),Some(key))=(&member.destination,&member.path_key) {
                    let (version,checksum)=if let Some(record)=&member.record {(record.version_size.as_str(),record.checksum.as_ref())} else if let Some(source)=&member.archive_source {(source.version.as_str(),source.provider_checksum.as_str())} else {return Err(invalid("destination has no rendition identity"));};
                    let foreign:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?1 AND (family<>?2 OR operation IS NOT NULL))",params![key,op.family],|r|r.get(0))?;
                    if foreign { return Err(invalid("foreign or pending primary path claim")); }
                    tx.execute("INSERT INTO primary_layout_claims(path_key,family,operation,library,child,version,provider_checksum,native_path) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(path_key) DO UPDATE SET operation=excluded.operation,version=excluded.version,provider_checksum=excluded.provider_checksum,local_checksum=NULL",params![key,op.family,op.operation,op.library,op.child,version,checksum,encode(destination)?])?;
                }
                if let Some(old)=&member.old {
                    let key=path_key(&old.path.to_path())?;
                    let foreign:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?1 AND family<>?2)",params![key,op.family],|r|r.get(0))?;
                    if foreign { return Err(invalid("foreign displaced path claim")); }
                    tx.execute("INSERT INTO primary_layout_claims(path_key,family,operation,library,child,version,provider_checksum,native_path) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(path_key) DO UPDATE SET operation=excluded.operation",params![key,op.family,op.operation,op.library,op.child,old.version,old.provider_checksum,encode(&old.path)?])?;
                }
            }
            tx.commit()?;Ok(())
        }).await
    }
    pub(crate) async fn cancel_primary_layout(&self, operation: String) -> Result<(), StateError> {
        self.with_conn("cancel_primary_layout",move |conn| {
            let tx=conn.unchecked_transaction()?;
            let header:Vec<u8>=tx.query_row("SELECT header FROM primary_layout_operations WHERE operation=?1 AND phase IN ('planned','prepared','preserved')",[&operation],|r|r.get(0))?;
            let op:LayoutOperation=decode(&header)?;
            // No visible publication is permitted before the durable publishing phase.
            tx.execute("DELETE FROM primary_layout_claims WHERE operation=?1",[&operation])?;
            if let Some(bytes)=tx.query_row("SELECT binding FROM primary_layout_bindings WHERE family=?1",[&op.family],|r|r.get::<_,Vec<u8>>(0)).optional()? {
                let binding:LayoutBinding=decode(&bytes)?;
                for file in binding.files {
                    tx.execute("INSERT INTO primary_layout_claims VALUES(?1,?2,NULL,?3,?4,?5,?6,?7,?8)",params![path_key(&file.path.to_path())?,op.family,op.library,op.child,file.version,file.provider_checksum,data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),encode(&file.path)?])?;
                }
            }
            tx.execute("UPDATE primary_layout_operations SET phase='cancelled' WHERE operation=?1",[operation])?;
            tx.commit()?;Ok(())
        }).await
    }
    /// Retain the recoverable phase and immutable member evidence. A fixed
    /// internal reason avoids persisting provider errors or identifiers.
    pub(crate) async fn hold_primary_layout(&self, operation: String) -> Result<(), StateError> {
        self.with_conn("hold_primary_layout",move |conn| {
            conn.execute("UPDATE primary_layout_operations SET conflict='verified_recovery_required' WHERE operation=?1 AND phase NOT IN ('committed','cancelled')",[operation])?;
            Ok(())
        }).await
    }
    pub(crate) async fn save_primary_layout(&self, op: LayoutOperation) -> Result<(), StateError> {
        self.with_conn("save_primary_layout", move |conn| {
            let tx=conn.unchecked_transaction()?;
            let previous:Vec<u8>=tx.query_row("SELECT header FROM primary_layout_operations WHERE operation=?1 AND phase NOT IN ('committed','cancelled')",[&op.operation],|row|row.get(0))?;
            let old:LayoutOperation=decode(&previous)?;
            let phase=|phase:&str|match phase {"planned"=>0,"prepared"=>1,"preserved"=>2,"publishing"=>3,_=>4};
            if phase(&op.phase)<phase(&old.phase) || phase(&op.phase)>phase(&old.phase)+1 {return Err(invalid("invalid layout phase transition"));}
            let mut identity=op.clone();identity.members.clear();identity.phase=old.phase.clone();
            if encode(&identity)?!=previous {return Err(invalid("planned layout identity cannot change"));}
            let count:i64=tx.query_row("SELECT COUNT(*) FROM primary_layout_members WHERE operation=?1",[&op.operation],|r|r.get(0))?;
            if usize::try_from(count).ok()!=Some(op.members.len()) {return Err(invalid("planned layout membership cannot change"));}
            let mut header=op.clone();header.members.clear();
            let changed=tx.execute("UPDATE primary_layout_operations SET phase=?1,header=?2,conflict=NULL WHERE operation=?3 AND phase NOT IN ('committed','cancelled')",params![op.phase,encode(&header)?,op.operation])?;
            if changed!=1 { return Err(invalid("layout operation is no longer pending")); }
            for (index,member) in op.members.iter().enumerate() {
                let prior:Vec<u8>=tx.query_row("SELECT evidence FROM primary_layout_members WHERE operation=?1 AND member=?2",params![op.operation,u32::try_from(index).map_err(|_error|invalid("layout member index exceeds budget"))?],|r|r.get(0))?;
                let old:LayoutMember=decode(&prior)?;
                let mut identity=member.clone();identity.prepared=old.prepared.clone();identity.preserved=old.preserved.clone();identity.installed=old.installed.clone();
                if encode(&identity)?!=prior {return Err(invalid("planned member identity cannot change"));}
                for (previous,next) in [(&old.prepared,&member.prepared),(&old.preserved,&member.preserved),(&old.installed,&member.installed)] {
                    if let Some(previous)=previous && next.as_ref().map(encode).transpose()?.as_ref()!=Some(&encode(previous)?) {return Err(invalid("recorded layout evidence cannot change"));}
                }
                if op.phase!="planned" && member.stage.is_some() && member.prepared.is_none() {return Err(invalid("layout member is not prepared"));}
                if matches!(op.phase.as_str(),"preserved"|"publishing") && member.old.is_some() && member.preserved.is_none() {return Err(invalid("layout member is not preserved"));}
                tx.execute("UPDATE primary_layout_members SET evidence=?1 WHERE operation=?2 AND member=?3",params![encode(member)?,op.operation,u32::try_from(index).map_err(|_error|invalid("layout member index exceeds budget"))?])?;
                if let Some(preserved)=&member.preserved {
                    let checksum=data_encoding::HEXLOWER.encode(&preserved.fingerprint.sha256);
                    let sidecar=preserved.sidecar.as_ref().map(|f|data_encoding::HEXLOWER.encode(&f.sha256));
                    let bytes=encode(preserved)?;
                    let prior:Option<Vec<u8>>=tx.query_row("SELECT evidence FROM primary_layout_preserved WHERE operation=?1 AND member=?2",params![op.operation,u32::try_from(index).map_err(|_error|invalid("layout member index exceeds budget"))?],|r|r.get(0)).optional()?;
                    if prior.is_some_and(|p|p!=bytes) { return Err(invalid("preservation receipt cannot change")); }
                    tx.execute("INSERT OR IGNORE INTO primary_layout_preserved VALUES(?1,?2,?3,?4,?5,?6)",params![op.operation,u32::try_from(index).map_err(|_error|invalid("layout member index exceeds budget"))?,encode(&preserved.path)?,checksum,sidecar,bytes])?;
                }
            }
            tx.commit()?;Ok(())
        }).await
    }
    pub(crate) async fn commit_primary_layout(
        &self,
        op: LayoutOperation,
    ) -> Result<(), StateError> {
        self.with_conn("commit_primary_layout", move |conn| {
            let tx=conn.unchecked_transaction()?;
            let generation:i64=tx.query_row("SELECT generation FROM primary_layout_bindings WHERE family=?1",[&op.family],|r|r.get(0)).optional()?.unwrap_or(0);
            if generation!=op.generation { return Err(invalid("primary layout generation changed before commit")); }
            let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_operations WHERE operation=?1 AND phase='publishing')",[&op.operation],|r|r.get(0))?;
            if !pending { return Err(invalid("layout publication has no durable journal")); }
            let stored:Vec<u8>=tx.query_row("SELECT header FROM primary_layout_operations WHERE operation=?1",[&op.operation],|r|r.get(0))?;
            let mut identity=op.clone();identity.members.clear();
            if encode(&identity)?!=stored {return Err(invalid("commit differs from recorded layout identity"));}
            let stored_members:Vec<Vec<u8>>=tx.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member")?.query_map([&op.operation],|r|r.get(0))?.collect::<Result<_,_>>()?;
            if stored_members.len()!=op.members.len() || op.members.iter().zip(stored_members).any(|(member,stored)|encode(member).map_or(true,|bytes|bytes!=stored)) {return Err(invalid("commit differs from recorded member evidence"));}
            let mut files=Vec::new();
            for member in &op.members {
                if let Some(old)=&member.old {
                    if member.preserved.is_none() { return Err(invalid("displaced file has no preservation receipt")); }
                    // Historical per-path metadata remains retained. Slot guards
                    // fence it from current use when a role is handed over.
                    let _=old;
                }
                if member.record.is_none() && member.retirement.is_none() && let Some(file)=&member.installed {
                    tx.execute("UPDATE primary_layout_claims SET operation=NULL,local_checksum=?3 WHERE path_key=?1 AND operation=?2",params![path_key(&file.path.to_path())?,op.operation,data_encoding::HEXLOWER.encode(&file.fingerprint.sha256)])?;
                    if member.archive_source.is_some() {
                        let current:Option<String>=tx.query_row("SELECT checksum FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",params![op.library,op.child,file.version],|row|row.get(0)).optional()?;
                        if current.as_deref()==Some(file.provider_checksum.as_str()) {
                            super::asset_writes::update_status_to_downloaded(&tx,&op.library,&op.child,&file.version,&file.path.to_path(),&data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),file.source_checksum.as_deref(),false,chrono::Utc::now().timestamp())?;
                        }
                    }
                    files.push(file.clone());
                }
                if let Some(record)=&member.record {
                    let installed=member.installed.as_ref().ok_or_else(||invalid("member is not installed"))?;
                    let path=installed.path.to_path();
                    // Release only this operation's claim before the atomic
                    // catalogue write. Other operations keep their fences.
                    tx.execute("UPDATE primary_layout_claims SET operation=NULL,local_checksum=?1 WHERE path_key=?2 AND operation=?3",params![data_encoding::HEXLOWER.encode(&installed.fingerprint.sha256),path_key(&path)?,op.operation])?;
                    super::asset_writes::upsert_asset_row(&tx,record,chrono::Utc::now().timestamp())?;
                    super::asset_writes::update_status_to_downloaded(&tx,&op.library,&op.child,record.version_size.as_str(),&path,&data_encoding::HEXLOWER.encode(&installed.fingerprint.sha256),installed.source_checksum.as_deref(),false,chrono::Utc::now().timestamp())?;
                    tx.execute("UPDATE assets SET metadata_write_failed_at=NULL WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4 AND checksum=?5 AND metadata_hash IS ?6",params![op.library,op.child,record.version_size.as_str(),path.to_string_lossy(),record.checksum.as_ref(),record.metadata.metadata_hash])?;
                    tx.execute("UPDATE asset_metadata_paths SET metadata_write_failed_at=NULL,source_checksum=?7 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4 AND provider_checksum=?5 AND EXISTS(SELECT 1 FROM assets a WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND a.metadata_hash IS ?6)",params![op.library,op.child,record.version_size.as_str(),path.to_string_lossy(),record.checksum.as_ref(),record.metadata.metadata_hash,installed.source_checksum])?;
                    super::provider_generations::record_layout_destination(&tx,record,installed,op.metadata_flags)?;
                    files.push(installed.clone());
                }
            }
            if op.members.iter().any(|member|member.record.is_some()) {
                // Match the ordinary verified-download finalizer: the selected
                // metadata was captured from the confirmed provider source and
                // committed with its independently validated files.
                super::asset_writes::record_metadata_capture_revision(
                    &tx,&op.library,&op.child,crate::state::METADATA_CAPTURE_REVISION,
                    chrono::Utc::now().timestamp(),
                )?;
            }
            for old in op.members.iter().filter_map(|member|member.old.as_ref()) {
                if !files.iter().any(|file|file.path==old.path && file.version==old.version && file.provider_checksum==old.provider_checksum) {
                    tx.execute("INSERT OR IGNORE INTO primary_layout_superseded_paths VALUES(?1,?2,?3,?4,?5,?6)",params![op.library,op.child,old.version,old.provider_checksum,encode(&old.path)?,old.path.to_path().to_str()])?;
                }
            }
            let binding=LayoutBinding{family:op.family.clone(),library:op.library.clone(),source_library:op.source_library.clone(),child:op.child.clone(),generation:generation+1,decision:op.decision.clone(),policy:op.policy.clone(),qualifier:op.qualifier.clone(),files};
            tx.execute("INSERT INTO primary_layout_bindings VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(family) DO UPDATE SET generation=excluded.generation,decision=excluded.decision,policy=excluded.policy,binding=excluded.binding",params![op.family,op.library,op.source_library,op.child,generation+1,op.decision,op.policy,encode(&binding)?])?;
            tx.execute("DELETE FROM primary_layout_claims WHERE operation=?1",[&op.operation])?;
            tx.execute("UPDATE primary_layout_operations SET phase='committed',conflict=NULL WHERE operation=?1",[op.operation])?;
            tx.commit()?;Ok(())
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LayoutBinding, LayoutFile, LayoutFingerprint, LayoutMember, LayoutOperation, decode,
        encode, path_key,
    };
    use crate::state::db::provider_selection::SelectionPath;
    use crate::state::{
        AssetMetadata, AssetRecord, AssetStatus, MediaType, SqliteStateDb, VersionSizeKey,
    };
    use std::sync::Arc;

    fn fixture(root: &std::path::Path) -> LayoutOperation {
        let identity_file = std::fs::File::create(root.join("identity")).unwrap();
        let fingerprint = LayoutFingerprint {
            size: 27,
            sha256: [7; 32],
            identity: crate::fs_util::file_identity(&identity_file).unwrap(),
        };
        let timestamp = chrono::DateTime::parse_from_rfc3339("2025-01-01T12:34:56.987654321+03:30")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut record = AssetRecord::new_pending(
            Arc::from("FullLibrary-Zone"),
            "stable-child".into(),
            VersionSizeKey::Adjusted,
            "new-provider-checksum".into(),
            "provider_original.HEIC".into(),
            timestamp,
            Some(timestamp),
            27,
            MediaType::LivePhotoImage,
        );
        record.local_path = Some(root.join("legacy_original.HEIC"));
        record.local_checksum = Some("local-old".into());
        record.download_checksum = Some("before-transform".into());
        record.last_error = Some("retained retry evidence".into());
        record.downloaded_at = Some(timestamp);
        record.last_seen_at = timestamp;
        record.download_attempts = 7;
        record.status = AssetStatus::Failed;
        record.metadata = Arc::new(AssetMetadata {
            source: Some(Arc::from("icloud")),
            is_favorite: true,
            rating: Some(5),
            latitude: Some(12.25),
            longitude: Some(-45.5),
            altitude: Some(300.25),
            orientation: Some(6),
            duration_secs: Some(2.5),
            timezone_offset: Some(12600),
            width: Some(4000),
            height: Some(3000),
            title: Some("題名".into()),
            keywords: Some("[\"one\",\"two\"]".into()),
            description: Some("caption\nsecond line".into()),
            media_subtype: Some("live".into()),
            burst_id: Some("burst".into()),
            is_hidden: true,
            is_archived: true,
            modified_at: Some(timestamp),
            is_deleted: true,
            deleted_at: Some(timestamp),
            provider_data: Some("{\"unknown\":{\"value\":17}}".into()),
            metadata_hash: Some("metadata-transform".into()),
        });
        let destination = root.join("provider_original.HEIC");
        let old = LayoutFile {
            path: SelectionPath::from_path(&destination),
            version: "original".into(),
            provider_checksum: "old-provider-checksum".into(),
            fingerprint: fingerprint.clone(),
            source_checksum: Some("old-source-checksum".into()),
            sidecar: Some(fingerprint.clone()),
            archive_original: false,
            metadata_decision: Some("old-metadata-transform".into()),
        };
        LayoutOperation {
            operation: "operation-1".into(),
            family: "family-1".into(),
            library: "FullLibrary-Zone".into(),
            source_library: "SelectedLibrary-Zone".into(),
            child: "stable-child".into(),
            asset_record_name: "provider-child".into(),
            pass: "album-pass".into(),
            metadata_flags: 3,
            refresh_metadata: true,
            root: SelectionPath::from_path(root),
            generation: 0,
            decision: "source-and-config-generation".into(),
            policy: "primary".into(),
            qualifier: "-stable-qualifier".into(),
            phase: "planned".into(),
            members: vec![LayoutMember {
                record: Some(record),
                destination: Some(SelectionPath::from_path(&destination)),
                path_key: Some(path_key(&destination).unwrap()),
                stage: Some(SelectionPath::from_path(&root.join(".prepared/new.HEIC"))),
                old: Some(old),
                archive_source: None,
                preserved_path: Some(SelectionPath::from_path(&root.join("history/old.HEIC"))),
                retirement: None,
                prepared: None,
                preserved: None,
                installed: None,
            }],
        }
    }

    #[test]
    fn primary_layout_complete_journal_and_binding_round_trip_retains_every_field() {
        let root = tempfile::tempdir().unwrap();
        let mut op = fixture(root.path());
        let old = op.members[0].old.clone().unwrap();
        op.members[0].archive_source = Some(old.clone());
        op.members[0].prepared = Some(old.clone());
        op.members[0].preserved = Some(old.clone());
        op.members[0].installed = Some(old.clone());
        op.members[0].retirement = Some(SelectionPath::from_path(
            &root.path().join("private-retirement"),
        ));
        let expected = serde_json::to_value(&op).unwrap();
        let decoded: LayoutOperation = decode(&encode(&op).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        let mut legacy = expected.clone();
        legacy.as_object_mut().unwrap().remove("refresh_metadata");
        let decoded_legacy: LayoutOperation =
            decode(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(
            !decoded_legacy.refresh_metadata,
            "older journals must not invent forced refresh intent"
        );
        assert_eq!(
            serde_json::to_value(decoded_legacy).unwrap(),
            legacy,
            "default refresh intent must preserve old journal identity"
        );
        let binding = LayoutBinding {
            family: op.family,
            library: op.library,
            source_library: op.source_library,
            child: op.child,
            generation: 7,
            decision: op.decision,
            policy: op.policy,
            qualifier: op.qualifier,
            files: vec![old],
        };
        let expected = serde_json::to_value(&binding).unwrap();
        let decoded: LayoutBinding = decode(&encode(&binding).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
        assert!(decode::<LayoutOperation>(b"malformed journal").is_err());
    }

    #[tokio::test]
    async fn primary_layout_journal_reopen_fences_membership_and_receipt_mutation() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let mut op = fixture(root.path());
        db.begin_primary_layout(op.clone()).await.unwrap();
        let mut wrong = op.clone();
        wrong.decision = "new generation".into();
        assert!(db.save_primary_layout(wrong).await.is_err());
        let mut wrong = op.clone();
        wrong.members[0].record.as_mut().unwrap().checksum = "different rendition".into();
        assert!(db.save_primary_layout(wrong).await.is_err());
        let mut wrong = op.clone();
        wrong.phase = "publishing".into();
        assert!(db.save_primary_layout(wrong).await.is_err());
        let old = op.members[0].old.clone().unwrap();
        op.members[0].prepared = Some(old.clone());
        db.save_primary_layout(op.clone()).await.unwrap();
        let mut wrong = op.clone();
        wrong.members[0]
            .prepared
            .as_mut()
            .unwrap()
            .fingerprint
            .sha256 = [9; 32];
        assert!(db.save_primary_layout(wrong).await.is_err());
        drop(db);
        let reopened = SqliteStateDb::open(&db_path).await.unwrap();
        let persisted = reopened
            .primary_layout_operations("SelectedLibrary-Zone".into())
            .await
            .unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(
            serde_json::to_value(&persisted[0]).unwrap(),
            serde_json::to_value(&op).unwrap()
        );
        assert!(
            reopened
                .primary_layout_operations("FullLibrary-Zone".into())
                .await
                .unwrap()
                .is_empty()
        );
        reopened.cancel_primary_layout(op.operation).await.unwrap();
        assert!(
            reopened
                .primary_layout_operations("SelectedLibrary-Zone".into())
                .await
                .unwrap()
                .is_empty()
        );
    }
}
