//! Explicit local-byte acceptance for preserved legacy migrations. Never source verification.
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{SqliteStateDb, legacy_preservation};
use crate::state::error::StateError;

const PREFIX: &str = "trusted_legacy_file:v1:";
pub(crate) const MAX_TARGETS: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrustedLegacyFile {
    pub(crate) library: String,
    pub(crate) child: String,
    pub(crate) master: String,
    pub(crate) rendition: String,
    pub(crate) path: PathBuf,
    pub(crate) local_sha256: String,
    /// Stable account, provider generation, identity, metadata and family evidence.
    pub(crate) binding: String,
    /// Actual local size, independent of provider size in binding.
    pub(crate) local_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LegacyReceiptProvenance {
    VerifiedSource,
    TrustedLegacyFile { receipt: String },
}

fn invalid() -> StateError {
    StateError::Invariant {
        operation: "trusted_legacy_file",
        detail: "trusted legacy file plan is invalid, stale or cancelled".into(),
    }
}
fn digest(value: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(value.as_bytes()))
}
fn prefix(library: &str) -> String {
    format!("{PREFIX}{}:", digest(library))
}
fn key(target: &TrustedLegacyFile) -> String {
    format!("{}{}", prefix(&target.library), digest(&target.binding))
}

pub(super) fn dependency_rows(
    conn: &Connection,
    library: &str,
) -> Result<Vec<(String, String)>, StateError> {
    let prefix = prefix(library);
    Ok(conn
        .prepare("SELECT key,value FROM metadata WHERE substr(key,1,length(?1))=?1 ORDER BY key")?
        .query_map([prefix], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

fn target(
    conn: &Connection,
    library: &str,
    child: &str,
    rendition: &str,
) -> Result<Option<TrustedLegacyFile>, StateError> {
    if !legacy_preservation::child_receipts_ready(conn, library, child)? {
        return Ok(None);
    }
    let row: Option<(String,String,String)> = conn.query_row(
        "SELECT m.master_record_name,a.local_path,a.local_checksum FROM assets a
         JOIN asset_master_mappings m ON m.library=a.library AND m.asset_record_name=a.id
         JOIN unattributed_legacy u ON u.library=m.library AND u.asset_id=m.master_record_name
         WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND a.id!=m.master_record_name
         AND length(a.local_checksum)=64 AND a.local_checksum NOT GLOB '*[^0-9a-f]*'
         AND NOT EXISTS(SELECT 1 FROM legacy_master_state_owners o WHERE o.library=a.library AND o.master_record_name=m.master_record_name)
         AND EXISTS(SELECT 1 FROM asset_metadata_paths p WHERE p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path AND p.provider_checksum=a.checksum AND p.local_checksum=a.local_checksum AND p.source_checksum IS NULL)",
        params![library,child,rendition], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((master, path, local_sha256)) = row else {
        return Ok(None);
    };
    // Scope never comes from the plan alone. Rebuild it from current owned state.
    let owner = legacy_preservation::snapshot_rows(
        conn,
        "SELECT * FROM account_owner WHERE ?1 IS NOT NULL AND ?2 IS NOT NULL",
        library,
        child,
    )?;
    let asset = legacy_preservation::snapshot_rows(
        conn,
        "SELECT library,id,version_size,checksum,size_bytes,created_at,added_at,metadata_hash,local_path,local_checksum,download_checksum FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size",
        library,
        child,
    )?;
    let paths = legacy_preservation::snapshot_rows(
        conn,
        "SELECT * FROM asset_metadata_paths WHERE library=?1 AND id=?2",
        library,
        child,
    )?;
    let family = legacy_preservation::snapshot_rows(
        conn,
        "SELECT library,asset_id,original_evidence FROM unattributed_legacy WHERE library=?1 AND asset_id=?2",
        library,
        &master,
    )?;
    let mappings = legacy_preservation::snapshot_rows(
        conn,
        "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE library=?1 AND (master_record_name=?2 OR asset_record_name=?2) ORDER BY asset_record_name",
        library,
        &master,
    )?;
    let revisions = legacy_preservation::snapshot_rows(
        conn,
        "SELECT library,asset_id,revision FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2",
        library,
        child,
    )?;
    let owners = legacy_preservation::snapshot_rows(
        conn,
        "SELECT * FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2",
        library,
        &master,
    )?;
    let binding = serde_json::json!([
        1, library, child, master, rendition, owner, asset, paths, family, mappings, revisions,
        owners
    ])
    .to_string();
    Ok(Some(TrustedLegacyFile {
        library: library.into(),
        child: child.into(),
        master,
        rendition: rendition.into(),
        path: path.into(),
        local_sha256,
        binding,
        local_size: 0,
    }))
}

pub(super) fn provenance(
    conn: &Connection,
    library: &str,
    child: &str,
    rendition: &str,
) -> Result<Option<LegacyReceiptProvenance>, StateError> {
    let verified: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path AND p.provider_checksum=a.checksum AND p.local_checksum=a.local_checksum WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3 AND length(p.source_checksum)=64 AND p.source_checksum NOT GLOB '*[^0-9a-f]*')",params![library,child,rendition],|r|r.get(0))?;
    if verified {
        return Ok(Some(LegacyReceiptProvenance::VerifiedSource));
    }
    // Preserve the strict no-acceptance path, including older synthetic stores
    // without owned-account headers. No receipt means no alternate provenance.
    let accepted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata WHERE substr(key,1,length(?1))=?1)",
        [prefix(library)],
        |row| row.get(0),
    )?;
    if !accepted {
        return Ok(None);
    }
    let Some(mut current) = target(conn, library, child, rendition)? else {
        return Ok(None);
    };
    let receipt = key(&current);
    let value: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key=?1", [&receipt], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(value) = value else { return Ok(None) };
    let accepted: TrustedLegacyFile =
        serde_json::from_str(&value).map_err(|_private_error| invalid())?;
    current.local_size = accepted.local_size;
    if current != accepted {
        return Ok(None);
    }
    Ok(Some(LegacyReceiptProvenance::TrustedLegacyFile { receipt }))
}

impl SqliteStateDb {
    pub(crate) async fn stored_trusted_legacy_file_count(&self) -> Result<u64, StateError> {
        self.with_conn("stored_trusted_legacy_file_count", |conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM metadata WHERE substr(key,1,length(?1))=?1",
                [PREFIX],
                |r| r.get(0),
            )?;
            u64::try_from(count).map_err(|_private_error| invalid())
        })
        .await
    }
    pub(crate) async fn trusted_legacy_candidates(
        &self,
        library: &str,
    ) -> Result<Vec<TrustedLegacyFile>, StateError> {
        let library = library.to_owned();
        self.with_conn("trusted_legacy_candidates",move |conn| {
            let ids=conn.prepare("SELECT a.id,a.version_size FROM assets a JOIN asset_master_mappings m ON m.library=a.library AND m.asset_record_name=a.id JOIN unattributed_legacy u ON u.library=m.library AND u.asset_id=m.master_record_name WHERE a.library=?1 ORDER BY a.id,a.version_size LIMIT ?2")?
                .query_map(params![library,(MAX_TARGETS+1) as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
            if ids.len()>MAX_TARGETS {return Err(invalid())}
            let mut targets=Vec::new();
            for (child,rendition) in ids {if let Some(t)=target(conn,&library,&child,&rendition)? {targets.push(t)}}
            Ok(targets)
        }).await
    }
    pub(crate) async fn apply_trusted_legacy_files<F>(
        &self,
        targets: Vec<TrustedLegacyFile>,
        validate_files: F,
    ) -> Result<(), StateError>
    where
        F: FnOnce() -> Result<(), StateError> + Send + 'static,
    {
        self.with_conn_mut("apply_trusted_legacy_files", move |conn| {
            if targets.is_empty() || targets.len() > MAX_TARGETS {
                return Err(invalid());
            }
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Append-only acceptances, including through generic metadata APIs.
            // Created only by explicit apply, in this same atomic transaction.
            tx.execute_batch("CREATE TRIGGER IF NOT EXISTS trusted_legacy_file_no_update BEFORE UPDATE ON metadata WHEN substr(OLD.key,1,length('trusted_legacy_file:v1:'))='trusted_legacy_file:v1:' OR substr(NEW.key,1,length('trusted_legacy_file:v1:'))='trusted_legacy_file:v1:' BEGIN SELECT RAISE(ABORT,'trusted legacy file acceptance is immutable'); END;
                CREATE TRIGGER IF NOT EXISTS trusted_legacy_file_no_delete BEFORE DELETE ON metadata WHEN substr(OLD.key,1,length('trusted_legacy_file:v1:'))='trusted_legacy_file:v1:' BEGIN SELECT RAISE(ABORT,'trusted legacy file acceptance is immutable'); END;
                CREATE TRIGGER IF NOT EXISTS trusted_legacy_file_no_replace BEFORE INSERT ON metadata WHEN substr(NEW.key,1,length('trusted_legacy_file:v1:'))='trusted_legacy_file:v1:' AND EXISTS(SELECT 1 FROM metadata WHERE key=NEW.key AND value!=NEW.value) BEGIN SELECT RAISE(ABORT,'trusted legacy file acceptance is immutable'); END;")?;
            let mut seen = std::collections::HashSet::new();
            for expected in &targets {
                let Some(mut current) =
                    target(&tx, &expected.library, &expected.child, &expected.rendition)?
                else {
                    return Err(invalid());
                };
                current.local_size = expected.local_size;
                if &current != expected || !seen.insert(key(expected)) {
                    return Err(invalid());
                }
                let value = serde_json::to_string(expected).map_err(|_private_error| invalid())?;
                tx.execute(
                    "INSERT OR IGNORE INTO metadata(key,value) VALUES(?1,?2)",
                    params![key(expected), value],
                )?;
                let stored: String = tx.query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [key(expected)],
                    |r| r.get(0),
                )?;
                if stored != value {
                    return Err(invalid());
                }
            }
            // Held confined handles and cancellation are checked after all SQL,
            // immediately before commit. Any error rolls back the entire plan.
            validate_files()?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub(crate) async fn legacy_child_provenance(
        &self,
        library: &str,
        child: &str,
        rendition: &str,
    ) -> Result<Option<LegacyReceiptProvenance>, StateError> {
        let (library, child, rendition) =
            (library.to_owned(), child.to_owned(), rendition.to_owned());
        self.with_conn("legacy_child_provenance", move |conn| {
            provenance(conn, &library, &child, &rendition)
        })
        .await
    }
}
