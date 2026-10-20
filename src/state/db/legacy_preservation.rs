//! Immutable legacy evidence and generation-fenced current-coverage receipts.
//! This owner never assigns identity, writes files or infers provider completion.

use std::path::PathBuf;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::SqliteStateDb;
use crate::state::error::StateError;

const EVIDENCE_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyPreparationSnapshot {
    pub(crate) library: String,
    pub(crate) asset_id: String,
    pub(crate) evidence: String,
    pub(crate) paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyFileEvidence {
    pub(crate) root: PathBuf,
    pub(crate) path: PathBuf,
    // None records an absent sidecar; absence must be revalidated too.
    pub(crate) sha256: Option<String>,
    pub(crate) size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyPreservation {
    pub(crate) library: String,
    pub(crate) asset_id: String,
    pub(crate) original_evidence: String,
    pub(crate) files: Vec<LegacyFileEvidence>,
    pub(crate) active_generation: Option<i64>,
    pub(crate) dependency_evidence: Option<String>,
    pub(crate) provider_evidence: Option<String>,
    pub(crate) config_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyActivationProof {
    pub(crate) library: String,
    pub(crate) asset_id: String,
    pub(crate) expected_original: String,
    pub(crate) expected_dependencies: String,
    pub(crate) provider_evidence: String,
    pub(crate) expected_metadata: Vec<(String, Option<String>)>,
    pub(crate) expected_active_generation: Option<i64>,
    pub(crate) config_hash: String,
    pub(crate) prior_cursor: String,
    pub(crate) next_cursor: String,
}

fn invalid() -> StateError {
    StateError::Invariant {
        operation: "legacy_preservation",
        detail: "legacy preservation evidence is invalid or changed".into(),
    }
}

// Identity ambiguity protects originals even when pending metadata work makes
// them ineligible for preservation preparation.
const AMBIGUOUS_MASTERS: &str = "SELECT library,master_record_name FROM asset_master_mappings m
    WHERE NOT EXISTS(SELECT 1 FROM legacy_master_state_owners o
        WHERE o.library=m.library AND o.master_record_name=m.master_record_name)
    AND NOT EXISTS(SELECT 1 FROM asset_master_mappings child
        WHERE child.library=m.library AND child.asset_record_name=m.master_record_name)
    GROUP BY library,master_record_name HAVING COUNT(*)>=2";

pub(super) fn protected_ids(
    conn: &Connection,
) -> Result<std::collections::HashSet<(String, String)>, StateError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT library,asset_id FROM unattributed_legacy UNION {AMBIGUOUS_MASTERS}"
    ))?;
    Ok(stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

mod path_cache;

pub(super) fn protected_paths(conn: &Connection) -> Result<Vec<PathBuf>, StateError> {
    path_cache::protected_paths(conn)
}

pub(super) fn path_is_protected(
    conn: &Connection,
    path: &std::path::Path,
) -> Result<bool, StateError> {
    path_cache::path_is_protected(conn, path)
}

pub(super) fn guard_writer(conn: &Connection, path: &std::path::Path) -> Result<(), StateError> {
    if path_is_protected(conn, path)? {
        return Err(StateError::Invariant {
            operation: "legacy_preservation",
            detail: "ambiguous legacy original requires preservation".into(),
        });
    }
    Ok(())
}

// Preserve SQLite values without lossy float/date formatting. Schema-versioned
// column names and values are part of the receipt, not executable SQL input.
pub(super) fn snapshot_rows(
    conn: &Connection,
    sql: &str,
    library: &str,
    id: &str,
) -> Result<Value, StateError> {
    let mut stmt = conn.prepare(sql)?;
    let columns: Vec<_> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let count = columns.len();
    let mut rows = stmt
        .query_map(params![library, id], |row| {
            let mut values = Vec::with_capacity(count);
            for i in 0..count {
                values.push(match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => json!(["null"]),
                    rusqlite::types::ValueRef::Integer(v) => json!(["integer", v]),
                    rusqlite::types::ValueRef::Real(v) => json!(["real_bits", v.to_bits()]),
                    rusqlite::types::ValueRef::Text(v) => {
                        json!(["text", data_encoding::HEXLOWER.encode(v)])
                    }
                    rusqlite::types::ValueRef::Blob(v) => {
                        json!(["blob", data_encoding::HEXLOWER.encode(v)])
                    }
                });
            }
            Ok(values)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.sort_by_cached_key(|row| json!(row).to_string());
    Ok(json!({"columns":columns,"rows":rows}))
}

fn original_evidence(conn: &Connection, library: &str, id: &str) -> Result<String, StateError> {
    let mut evidence = serde_json::Map::new();
    for (table, key) in [
        ("assets", "id"),
        ("asset_metadata_paths", "id"),
        ("asset_metadata_capture_revisions", "asset_id"),
        ("metadata_capture_retries", "asset_id"),
        ("asset_albums", "asset_id"),
        ("asset_people", "asset_id"),
        ("asset_album_memberships", "asset_record_name"),
        ("asset_verifications", "id"),
    ] {
        // Literal table/key pairs above, never provider or user strings.
        evidence.insert(
            table.into(),
            snapshot_rows(
                conn,
                &format!("SELECT * FROM {table} WHERE library=?1 AND {key}=?2 ORDER BY 1,2,3"),
                library,
                id,
            )?,
        );
    }
    evidence.insert(
        "capture_state_at_preparation".into(),
        snapshot_rows(
            conn,
            "SELECT * FROM metadata_capture_state WHERE library=?1 AND ?2 IS NOT NULL",
            library,
            id,
        )?,
    );
    evidence.insert("family_history".into(), snapshot_rows(conn,
        "SELECT * FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2 ORDER BY asset_record_name", library,id)?);
    evidence.insert("owners".into(), snapshot_rows(conn,
        "SELECT * FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2 ORDER BY asset_record_name",library,id)?);
    Ok(
        json!({"version":EVIDENCE_VERSION,"library":library,"asset_id":id,"evidence":evidence})
            .to_string(),
    )
}

pub(super) fn dependencies(
    conn: &Connection,
    library: &str,
    id: &str,
) -> Result<String, StateError> {
    let children = snapshot_rows(
        conn,
        "SELECT a.library,a.id,a.version_size,a.checksum,a.size_bytes,a.status,a.local_path,a.local_checksum,a.download_checksum,a.created_at,a.added_at,a.metadata_hash,a.is_deleted,a.is_hidden,a.capture_repair_metadata_hash,a.metadata_write_failed_at FROM assets a WHERE a.library=?1 AND a.id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2) ORDER BY a.id,a.version_size",
        library,
        id,
    )?;
    let mappings = snapshot_rows(
        conn,
        "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE library=?1 AND (master_record_name=?2 OR asset_record_name=?2) ORDER BY asset_record_name",
        library,
        id,
    )?;
    let owners = snapshot_rows(
        conn,
        "SELECT * FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2 ORDER BY asset_record_name",
        library,
        id,
    )?;
    let mut additional = Vec::new();
    for (table, key) in [
        ("asset_metadata_paths", "id"),
        ("reconciliation_paths", "id"),
    ] {
        additional.push(snapshot_rows(conn, &format!("SELECT * FROM {table} WHERE library=?1 AND {key} IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2)"),library,id)?);
    }
    additional.push(snapshot_rows(conn,"SELECT library,asset_id,revision FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id IN (SELECT asset_record_name FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2)",library,id)?);
    let trusted = super::trusted_legacy::dependency_rows(conn, library)?;
    if !trusted.is_empty() {
        additional.push(json!({"trusted_legacy_files_v1":trusted}));
    }
    Ok(json!([EVIDENCE_VERSION, children, mappings, owners, additional]).to_string())
}

fn preparation_snapshot(
    conn: &Connection,
    library: &str,
    id: &str,
) -> Result<Option<LegacyPreparationSnapshot>, StateError> {
    let eligible: bool = conn.query_row(r"SELECT
        EXISTS(SELECT 1 FROM metadata_capture_retries WHERE library=?1 AND asset_id=?2)
        AND EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2)
        AND NOT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND
            (status!='downloaded' OR is_deleted!=0 OR local_path IS NULL OR metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL))
        AND NOT EXISTS(SELECT 1 FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND
            (metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL))
        AND NOT EXISTS(SELECT 1 FROM legacy_master_state_owners WHERE library=?1 AND master_record_name=?2)
        AND NOT EXISTS(SELECT 1 FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2)
        AND (SELECT COUNT(*) FROM asset_master_mappings WHERE library=?1 AND master_record_name=?2)>=2
        AND NOT EXISTS(SELECT 1 FROM reconciliation_paths WHERE library=?1 AND id=?2)
        AND NOT EXISTS(SELECT 1 FROM asset_verifications WHERE library=?1 AND id=?2)
        AND NOT EXISTS(SELECT 1 FROM unattributed_legacy WHERE library=?1 AND asset_id=?2)",params![library,id],|r|r.get(0))?;
    if !eligible {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT local_path FROM assets WHERE library=?1 AND id=?2 UNION SELECT local_path FROM asset_metadata_paths WHERE library=?1 AND id=?2 ORDER BY local_path")?;
    let paths = stmt
        .query_map(params![library, id], |row| {
            Ok(PathBuf::from(row.get::<_, String>(0)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(LegacyPreparationSnapshot {
        library: library.to_owned(),
        asset_id: id.to_owned(),
        evidence: original_evidence(conn, library, id)?,
        paths,
    }))
}

/// Diagnostic-only snapshot after an already refused selected receipt. It never
/// grants qualification, even if another writer has since made the row eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyChildReceiptRefusal {
    ChildNotReady,
    RenditionMissing,
    PathReceiptMissing,
    BindingMismatch,
    SourceMissing,
    SourceInvalid,
    StateChanged,
    Unavailable,
}

impl LegacyChildReceiptRefusal {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ChildNotReady => "child_not_ready",
            Self::RenditionMissing => "rendition_missing",
            Self::PathReceiptMissing => "path_receipt_missing",
            Self::BindingMismatch => "receipt_binding_mismatch",
            Self::SourceMissing => "source_receipt_missing",
            Self::SourceInvalid => "source_receipt_invalid",
            Self::StateChanged => "receipt_state_changed",
            Self::Unavailable => "diagnostic_unavailable",
        }
    }
}

pub(super) fn child_receipts_ready(
    conn: &Connection,
    library: &str,
    id: &str,
) -> Result<bool, StateError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2 AND revision>=?3) AND NOT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND (status!='downloaded' OR is_deleted!=0 OR metadata_hash IS NULL OR metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL)) AND NOT EXISTS(SELECT 1 FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND (metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL))", params![library, id, crate::state::METADATA_CAPTURE_REVISION], |row| row.get(0))?)
}

impl SqliteStateDb {
    /// Return finalized current-child receipts with verified source provenance or
    /// explicit Trusted Legacy File acceptance for this exact generation/binding.
    /// Reconciliation reservations are retained naming evidence, not pending
    /// work. They remain part of the dependency fence; the sync owner must
    /// independently prove reconciliation completed before certification.
    pub(crate) async fn legacy_child_receipts(
        &self,
        library: &str,
        id: &str,
    ) -> Result<Vec<crate::state::AssetRecord>, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        self.with_conn("legacy_child_receipts", move |conn| {
            let ready = child_receipts_ready(conn, &library, &id)?;
            if !ready {
                return Ok(Vec::new());
            }
            let mut stmt = conn.prepare(&format!(
                "SELECT {} FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size",
                super::rows::ASSET_COLUMNS
            ))?;
            let rows = stmt
                .query_map(params![library, id], super::rows::row_to_asset_record)?
                .collect::<Result<Vec<_>, _>>()?;
            let mut accepted = Vec::new();
            for row in rows {
                if super::trusted_legacy::provenance(
                    conn,
                    &library,
                    &id,
                    row.version_size.as_str(),
                )?
                .is_some()
                {
                    accepted.push(row);
                }
            }
            Ok(accepted)
        })
        .await
    }

    /// Explain a missing selected receipt without writing state or changing the
    /// acceptance query. These observations can differ from the preceding read.
    pub(crate) async fn legacy_child_receipt_refusal(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<LegacyChildReceiptRefusal, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn("legacy_child_receipt_refusal", move |conn| {
            if !child_receipts_ready(conn, &library, &id)? {
                return Ok(LegacyChildReceiptRefusal::ChildNotReady);
            }
            let rendition: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3)",
                params![library, id, version_size], |row| row.get(0),
            )?;
            if !rendition {
                return Ok(LegacyChildReceiptRefusal::RenditionMissing);
            }
            let receipt: Option<(bool, bool)> = conn.query_row(
                "SELECT p.source_checksum IS NOT NULL,
                        COALESCE(length(p.source_checksum)=64 AND p.source_checksum NOT GLOB '*[^0-9a-f]*', 0)
                 FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id
                   AND p.version_size=a.version_size AND p.local_path=a.local_path
                   AND p.provider_checksum=a.checksum AND p.local_checksum=a.local_checksum
                 WHERE a.library=?1 AND a.id=?2 AND a.version_size=?3",
                params![library, id, version_size], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            Ok(match receipt {
                Some((_, true)) => LegacyChildReceiptRefusal::StateChanged,
                Some((false, false)) => LegacyChildReceiptRefusal::SourceMissing,
                Some((true, false)) => LegacyChildReceiptRefusal::SourceInvalid,
                None => {
                    let scoped_path: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3)",
                        params![library, id, version_size], |row| row.get(0),
                    )?;
                    if scoped_path {
                        LegacyChildReceiptRefusal::BindingMismatch
                    } else {
                        LegacyChildReceiptRefusal::PathReceiptMissing
                    }
                }
            })
        }).await
    }

    pub(crate) async fn legacy_preparation_snapshots(
        &self,
        library: &str,
        limit: usize,
    ) -> Result<Vec<LegacyPreparationSnapshot>, StateError> {
        let library = library.to_owned();
        self.with_conn("legacy_preparation_snapshots", move |conn| {
            let mut stmt=conn.prepare("SELECT DISTINCT asset_id FROM metadata_capture_retries WHERE library=?1 AND NOT EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=metadata_capture_retries.library AND p.asset_id=metadata_capture_retries.asset_id) ORDER BY asset_id LIMIT ?2")?;
            let ids=stmt.query_map(params![library,i64::try_from(limit).unwrap_or(i64::MAX)],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
            let mut snapshots=Vec::new();
            for id in ids { if let Some(snapshot)=preparation_snapshot(conn,&library,&id)? { snapshots.push(snapshot); } }
            Ok(snapshots)
        }).await
    }

    pub(crate) async fn prepare_legacy_preservation(
        &self,
        expected: &LegacyPreparationSnapshot,
        files: &[LegacyFileEvidence],
    ) -> Result<bool, StateError> {
        let expected = expected.clone();
        let files = files.to_vec();
        self.with_conn_mut("prepare_legacy_preservation",move |conn| {
            let tx=conn.transaction()?;
            let Some(current)=preparation_snapshot(&tx,&expected.library,&expected.asset_id)? else { return Ok(false); };
            if current!=expected { return Ok(false); }
            let mut keys=std::collections::HashSet::new();
            let foreign_paths = {
                let mut paths=tx.prepare("SELECT library,id,local_path FROM assets WHERE local_path IS NOT NULL UNION SELECT library,id,local_path FROM asset_metadata_paths")?;
                let catalog=paths.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?;
                let mut foreign_paths = std::collections::HashSet::new();
                for row in catalog {
                    let (library,id,path)=row?;
                    if library!=expected.library || id!=expected.asset_id {
                        foreign_paths.insert(crate::fs_util::confined_path_key(std::path::Path::new(&path)).map_err(|_private_error|invalid())?);
                    }
                }
                foreign_paths
            };
            for file in &files {
                if !file.path.is_absolute() || !file.root.is_absolute() || !file.path.starts_with(&file.root)
                    || file.sha256.is_some()!=file.size.is_some()
                    || file.sha256.as_ref().is_some_and(|hash| hash.len()!=64 || !hash.bytes().all(|c|c.is_ascii_hexdigit())) { return Err(invalid()); }
                let key=crate::fs_util::confined_path_key(&file.path).map_err(|_private_error|invalid())?;
                if !keys.insert(key) { return Err(invalid()); }
            }
            // Every media path and its exact sidecar name must be represented.
            let required: std::collections::HashSet<_>=expected.paths.iter().flat_map(|path| {
                let mut sidecar = path.as_os_str().to_os_string();
                sidecar.push(".xmp");
                [path.clone(), PathBuf::from(sidecar)]
            }).collect();
            if files.len()!=required.len() || files.iter().any(|file| !required.contains(&file.path))
                || expected.paths.iter().any(|path| !files.iter().any(|file| &file.path==path && file.sha256.is_some())) { return Err(invalid()); }
            if keys.iter().any(|key| foreign_paths.contains(key)) {
                tracing::warn!(
                    stage = "preparation",
                    reason = "catalog_path_conflict",
                    "Legacy preservation path is claimed by another catalogue asset; retaining attribution checkpoint hold"
                );
                return Ok(false);
            }
            let encoded=serde_json::to_string(&files).map_err(|_private_error|invalid())?;
            tx.execute("INSERT INTO unattributed_legacy(library,asset_id,evidence_version,original_evidence,files,prepared_at) VALUES (?1,?2,1,?3,?4,?5)",params![expected.library,expected.asset_id,expected.evidence,encoded,Utc::now().timestamp()])?;
            for file in files {
                tx.execute("INSERT INTO unattributed_legacy_paths VALUES (?1,?2,?3,?4)",params![expected.library,expected.asset_id,crate::fs_util::confined_path_key(&file.path).map_err(|_private_error|invalid())?,file.path.to_str().ok_or_else(invalid)?])?;
            }
            tx.commit()?;
            Ok(true)
        }).await
    }

    pub(crate) async fn legacy_preservations(
        &self,
        library: &str,
    ) -> Result<Vec<LegacyPreservation>, StateError> {
        let library = library.to_owned();
        self.with_conn("legacy_preservations",move |conn| {
            let mut stmt=conn.prepare("SELECT p.asset_id,p.evidence_version,p.original_evidence,p.files,p.active_generation,r.dependency_evidence,r.config_hash,r.evidence_version,r.provider_evidence FROM unattributed_legacy p LEFT JOIN unattributed_legacy_proofs r ON r.library=p.library AND r.asset_id=p.asset_id AND r.generation=p.active_generation WHERE p.library=?1 ORDER BY p.asset_id")?;
            let mut rows=stmt.query([&library])?; let mut result=Vec::new();
            while let Some(row)=rows.next()? {
                let active:Option<i64>=row.get(4)?;
                if row.get::<_,i64>(1)?!=EVIDENCE_VERSION || (active.is_some() && row.get::<_,Option<i64>>(7)?!=Some(EVIDENCE_VERSION)) { return Err(invalid()); }
                let files=serde_json::from_str(&row.get::<_,String>(3)?).map_err(|_private_error|invalid())?;
                result.push(LegacyPreservation {library:library.clone(),asset_id:row.get(0)?,original_evidence:row.get(2)?,files,active_generation:active,dependency_evidence:row.get(5)?,config_hash:row.get(6)?,provider_evidence:row.get(8)?});
            }
            Ok(result)
        }).await
    }

    pub(crate) async fn legacy_dependency_evidence(
        &self,
        library: &str,
        id: &str,
    ) -> Result<String, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        self.with_conn("legacy_dependency_evidence", move |conn| {
            dependencies(conn, &library, &id)
        })
        .await
    }

    pub(crate) async fn reactivate_legacy_preservation(
        &self,
        library: &str,
        id: &str,
        expected_generation: Option<i64>,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        self.with_conn("reactivate_legacy_preservation",move |conn| {
            let changed=conn.execute("UPDATE unattributed_legacy SET active_generation=NULL WHERE library=?1 AND asset_id=?2 AND active_generation IS ?3",params![library,id,expected_generation])?;
            if changed!=1 { return Err(invalid()); }
            Ok(())
        }).await
    }
}

// Called only inside the cursor transaction, after normal current-work proof.
pub(super) fn activate(conn: &Connection, proof: &LegacyActivationProof) -> Result<(), StateError> {
    if proof.config_hash.trim().is_empty()
        || proof.next_cursor.trim().is_empty()
        || proof.provider_evidence.trim().is_empty()
    {
        return Err(invalid());
    }
    for (key, expected) in &proof.expected_metadata {
        let current: Option<String> = conn
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        if &current != expected {
            return Err(invalid());
        }
    }
    let current:Option<(i64,String,Option<i64>)>=conn.query_row("SELECT evidence_version,original_evidence,active_generation FROM unattributed_legacy WHERE library=?1 AND asset_id=?2",params![proof.library,proof.asset_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if current
        != Some((
            EVIDENCE_VERSION,
            proof.expected_original.clone(),
            proof.expected_active_generation,
        ))
        || dependencies(conn, &proof.library, &proof.asset_id)? != proof.expected_dependencies
    {
        return Err(invalid());
    }
    let prior: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            [format!("sync_token:{}", proof.library)],
            |r| r.get(0),
        )
        .optional()?;
    if prior.as_deref().unwrap_or("") != proof.prior_cursor {
        return Err(invalid());
    }
    let generation:i64=conn.query_row("SELECT COALESCE(MAX(generation),0) FROM unattributed_legacy_proofs WHERE library=?1 AND asset_id=?2",params![proof.library,proof.asset_id],|r|r.get(0))?;
    let generation = generation.checked_add(1).ok_or_else(invalid)?;
    conn.execute(
        "INSERT INTO unattributed_legacy_proofs VALUES (?1,?2,?3,1,?4,?5,?6,?7,?8,?9)",
        params![
            proof.library,
            proof.asset_id,
            generation,
            proof.expected_dependencies,
            proof.provider_evidence,
            proof.config_hash,
            proof.prior_cursor,
            proof.next_cursor,
            Utc::now().timestamp()
        ],
    )?;
    conn.execute(
        "UPDATE unattributed_legacy SET active_generation=?3 WHERE library=?1 AND asset_id=?2",
        params![proof.library, proof.asset_id, generation],
    )?;
    Ok(())
}

pub(super) fn checkpoint_library(key: &str) -> Option<&str> {
    key.strip_prefix("sync_token:").or_else(|| {
        key.strip_prefix("pending_sync_token:")
            .and_then(|key| key.split_once(':').map(|(_, library)| library))
    })
}

pub(super) fn invalid_checkpoint() -> StateError {
    invalid()
}

pub(super) fn validate_checkpoint(
    conn: &Connection,
    library: &str,
    config_hash: Option<&str>,
) -> Result<(), StateError> {
    let mut stmt=conn.prepare("SELECT p.asset_id,p.evidence_version,p.active_generation,r.evidence_version,r.dependency_evidence,r.config_hash FROM unattributed_legacy p LEFT JOIN unattributed_legacy_proofs r ON r.library=p.library AND r.asset_id=p.asset_id AND r.generation=p.active_generation WHERE p.library=?1 ORDER BY p.asset_id")?;
    let mut rows = stmt.query([library])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if row.get::<_, i64>(1)? != EVIDENCE_VERSION
            || row.get::<_, Option<i64>>(2)?.is_none()
            || row.get::<_, Option<i64>>(3)? != Some(EVIDENCE_VERSION)
            || row.get::<_, Option<String>>(5)?.as_deref() != config_hash
            || row.get::<_, Option<String>>(4)?.as_deref()
                != Some(dependencies(conn, library, &id)?.as_str())
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Read-only status includes locally invalidated coverage before the next cycle
/// persists reactivation. This does not certify file contents or provider state.
pub(super) fn counts(conn: &Connection, library: &str) -> Result<(u64, u64), StateError> {
    let mut stmt=conn.prepare("SELECT p.asset_id,p.evidence_version,p.active_generation,r.evidence_version,r.dependency_evidence FROM unattributed_legacy p LEFT JOIN unattributed_legacy_proofs r ON r.library=p.library AND r.asset_id=p.asset_id AND r.generation=p.active_generation WHERE p.library=?1")?;
    let mut rows = stmt.query([library])?;
    let mut total = 0;
    let mut pending = 0;
    while let Some(row) = rows.next()? {
        total += 1;
        let id: String = row.get(0)?;
        if row.get::<_, i64>(1)? != EVIDENCE_VERSION
            || row.get::<_, Option<i64>>(2)?.is_none()
            || row.get::<_, Option<i64>>(3)? != Some(EVIDENCE_VERSION)
            || row.get::<_, Option<String>>(4)?.as_deref()
                != Some(dependencies(conn, library, &id)?.as_str())
        {
            pending += 1;
        }
    }
    Ok((total, pending))
}

#[cfg(test)]
mod tests {
    use super::{
        LegacyActivationProof, LegacyFileEvidence, LegacyPreparationSnapshot, SqliteStateDb,
    };
    use crate::state::CheckpointTransition;
    use std::path::Path;

    #[tokio::test]
    async fn legacy_receipt_prerequisites_are_read_only_after_reopen() {
        use super::LegacyChildReceiptRefusal;
        for (fault, expected) in [
            ("ready", LegacyChildReceiptRefusal::StateChanged),
            ("missing_capture", LegacyChildReceiptRefusal::ChildNotReady),
            ("stale_capture", LegacyChildReceiptRefusal::ChildNotReady),
            ("pending", LegacyChildReceiptRefusal::ChildNotReady),
            ("deleted", LegacyChildReceiptRefusal::ChildNotReady),
            ("metadata", LegacyChildReceiptRefusal::ChildNotReady),
            ("asset_debt", LegacyChildReceiptRefusal::ChildNotReady),
            ("path_debt", LegacyChildReceiptRefusal::ChildNotReady),
            ("prepared_debt", LegacyChildReceiptRefusal::ChildNotReady),
            ("rendition", LegacyChildReceiptRefusal::RenditionMissing),
            (
                "path_receipt",
                LegacyChildReceiptRefusal::PathReceiptMissing,
            ),
            (
                "foreign_library",
                LegacyChildReceiptRefusal::PathReceiptMissing,
            ),
            (
                "foreign_child",
                LegacyChildReceiptRefusal::PathReceiptMissing,
            ),
            (
                "foreign_rendition",
                LegacyChildReceiptRefusal::PathReceiptMissing,
            ),
            ("path", LegacyChildReceiptRefusal::BindingMismatch),
            ("provider", LegacyChildReceiptRefusal::BindingMismatch),
            ("local", LegacyChildReceiptRefusal::BindingMismatch),
            ("null_local", LegacyChildReceiptRefusal::BindingMismatch),
            ("source_missing", LegacyChildReceiptRefusal::SourceMissing),
            ("source_empty", LegacyChildReceiptRefusal::SourceInvalid),
            ("source_short", LegacyChildReceiptRefusal::SourceInvalid),
            ("source_upper", LegacyChildReceiptRefusal::SourceInvalid),
            ("source_nonhex", LegacyChildReceiptRefusal::SourceInvalid),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("state.db");
            let media = dir.path().join("private-current.jpg");
            let bytes = b"independent verified source";
            std::fs::write(&media, bytes).unwrap();
            let hash = crate::download::file::compute_sha256(&media).await.unwrap();
            let db = SqliteStateDb::open(&database).await.unwrap();
            db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("private-child").build())
                .await
                .unwrap();
            crate::state::DownloadStateStore::mark_verified_download(
                &db,
                "PrimarySync",
                "private-child",
                "original",
                &media,
                &hash,
                Some(&hash),
                false,
            )
            .await
            .unwrap();
            {
                let conn = db.acquire_lock("prerequisite fixture").unwrap();
                let sql = match fault {
                    "missing_capture" => "DELETE FROM asset_metadata_capture_revisions",
                    "stale_capture" => "UPDATE asset_metadata_capture_revisions SET revision=0",
                    "pending" => "UPDATE assets SET status='pending'",
                    "deleted" => "UPDATE assets SET is_deleted=1",
                    "metadata" => "UPDATE assets SET metadata_hash=NULL",
                    "asset_debt" => "UPDATE assets SET metadata_write_failed_at=1",
                    "path_debt" => {
                        "UPDATE asset_metadata_paths SET capture_repair_metadata_hash='private-debt'"
                    }
                    "prepared_debt" => {
                        "UPDATE asset_metadata_paths SET capture_repair_output_size=1"
                    }
                    "path_receipt" => "DELETE FROM asset_metadata_paths",
                    "foreign_library" => {
                        "UPDATE asset_metadata_paths SET library='private-foreign-library'"
                    }
                    "foreign_child" => "UPDATE asset_metadata_paths SET id='private-foreign-child'",
                    "foreign_rendition" => {
                        "UPDATE asset_metadata_paths SET version_size='adjusted'"
                    }
                    "path" => {
                        "UPDATE asset_metadata_paths SET local_path=local_path || '.private-other'"
                    }
                    "provider" => {
                        "UPDATE asset_metadata_paths SET provider_checksum='private-other-generation'"
                    }
                    "local" => {
                        "UPDATE asset_metadata_paths SET local_checksum='private-other-bytes'"
                    }
                    "null_local" => "UPDATE asset_metadata_paths SET local_checksum=NULL",
                    "source_missing" => "UPDATE asset_metadata_paths SET source_checksum=NULL",
                    "source_empty" => "UPDATE asset_metadata_paths SET source_checksum=''",
                    "source_short" => "UPDATE asset_metadata_paths SET source_checksum='abc'",
                    "source_upper" => {
                        "UPDATE asset_metadata_paths SET source_checksum=upper(source_checksum)"
                    }
                    "source_nonhex" => {
                        "UPDATE asset_metadata_paths SET source_checksum=replace(source_checksum,substr(source_checksum,1,1),'z')"
                    }
                    _ => "SELECT 1",
                };
                conn.execute_batch(sql).unwrap();
            }
            drop(db);
            let db = SqliteStateDb::open(&database).await.unwrap();
            let version = if fault == "rendition" {
                "adjusted"
            } else {
                "original"
            };
            let before = db
                .legacy_child_receipts("PrimarySync", "private-child")
                .await
                .unwrap();
            assert_eq!(
                before
                    .iter()
                    .any(|row| row.version_size.as_str() == version),
                fault == "ready",
                "eligibility {fault}"
            );
            // Even diagnostic fallbacks must work on a query-only connection.
            let changes: i64 = {
                let conn = db.acquire_lock("query-only diagnostic").unwrap();
                conn.pragma_update(None, "query_only", true).unwrap();
                conn.query_row("SELECT total_changes()", [], |row| row.get(0))
                    .unwrap()
            };
            for _ in 0..2 {
                assert_eq!(
                    db.legacy_child_receipt_refusal("PrimarySync", "private-child", version)
                        .await
                        .unwrap(),
                    expected,
                    "{fault}"
                );
            }
            let after = db
                .legacy_child_receipts("PrimarySync", "private-child")
                .await
                .unwrap();
            assert_eq!(before.len(), after.len(), "qualification unchanged {fault}");
            assert_eq!(
                db.acquire_lock("unchanged diagnostic")
                    .unwrap()
                    .query_row::<i64, _, _>("SELECT total_changes()", [], |row| row.get(0))
                    .unwrap(),
                changes,
                "no writes {fault}"
            );
            assert_eq!(
                std::fs::read(&media).unwrap(),
                bytes,
                "media unchanged {fault}"
            );
        }
    }

    #[tokio::test]
    async fn legacy_current_receipts_require_exact_verified_source_after_reopen() {
        for fault in [
            "none",
            "historical",
            "library",
            "child",
            "rendition",
            "checked_rewrite",
            "malformed",
            "generation",
            "path",
            "bytes",
            "debt",
            "unverified_replacement",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let media = dir.path().join("current.jpg");
            std::fs::write(&media, b"verified current source").unwrap();
            let hash = crate::download::file::compute_sha256(&media).await.unwrap();
            let db = SqliteStateDb::open(&path).await.unwrap();
            let record = crate::test_helpers::TestAssetRecord::new("current-child")
                .checksum("cnJycnJycnJycnJycnJycnJycnJy")
                .build();
            db.upsert_seen(&record).await.unwrap();
            if fault == "historical" {
                // Equal historical hashes never establish verified provenance.
                db.mark_downloaded(
                    "PrimarySync",
                    "current-child",
                    "original",
                    &media,
                    &hash,
                    Some(&hash),
                )
                .await
                .unwrap();
            } else {
                crate::state::DownloadStateStore::mark_verified_download(
                    &db,
                    "PrimarySync",
                    "current-child",
                    "original",
                    &media,
                    &hash,
                    Some(&hash),
                    false,
                )
                .await
                .unwrap();
            }
            {
                let conn = db.acquire_lock("source provenance fault").unwrap();
                match fault {
                    "library" => {
                        conn.execute("UPDATE asset_metadata_paths SET library='OtherLibrary'", [])
                            .unwrap();
                    }
                    "child" => {
                        conn.execute("UPDATE asset_metadata_paths SET id='other-child'", [])
                            .unwrap();
                    }
                    "rendition" => {
                        conn.execute(
                            "UPDATE asset_metadata_paths SET version_size='adjusted'",
                            [],
                        )
                        .unwrap();
                    }
                    "malformed" => {
                        conn.execute(
                            "UPDATE asset_metadata_paths SET source_checksum='not-a-source-hash'",
                            [],
                        )
                        .unwrap();
                    }
                    "generation" => {
                        conn.execute("UPDATE asset_metadata_paths SET provider_checksum='different-generation'", []).unwrap();
                    }
                    "path" => {
                        conn.execute(
                            "UPDATE asset_metadata_paths SET local_path=local_path || '.other'",
                            [],
                        )
                        .unwrap();
                    }
                    "bytes" => {
                        conn.execute("UPDATE asset_metadata_paths SET local_checksum='different-local-bytes'", []).unwrap();
                    }
                    "debt" => {
                        conn.execute("UPDATE asset_metadata_paths SET capture_repair_metadata_hash='pending'", []).unwrap();
                    }
                    _ => {}
                }
            }
            if fault == "checked_rewrite" {
                std::fs::write(&media, b"checked metadata rewrite").unwrap();
                let changed = crate::download::file::compute_sha256(&media).await.unwrap();
                let conn = db.acquire_lock("checked writer completion").unwrap();
                conn.execute("UPDATE assets SET local_checksum=?1", [&changed])
                    .unwrap();
                super::super::asset_writes::record_metadata_path(
                    &conn,
                    "PrimarySync",
                    "current-child",
                    "original",
                    super::super::asset_writes::MetadataPathWrite::Rewritten,
                )
                .unwrap();
            }
            if fault == "unverified_replacement" {
                std::fs::write(&media, b"locally replaced bytes").unwrap();
                let changed = crate::download::file::compute_sha256(&media).await.unwrap();
                db.mark_downloaded(
                    "PrimarySync",
                    "current-child",
                    "original",
                    &media,
                    &changed,
                    Some(&hash),
                )
                .await
                .unwrap();
                assert!(
                    db.acquire_lock("unknown replacement source")
                        .unwrap()
                        .query_row::<Option<String>, _, _>(
                            "SELECT source_checksum FROM asset_metadata_paths",
                            [],
                            |row| row.get(0)
                        )
                        .unwrap()
                        .is_none()
                );
            }
            drop(db);
            let reopened = SqliteStateDb::open(&path).await.unwrap();
            assert_eq!(
                reopened
                    .legacy_child_receipts("PrimarySync", "current-child")
                    .await
                    .unwrap()
                    .len(),
                usize::from(matches!(fault, "none" | "checked_rewrite")),
                "{fault}"
            );
            assert!(
                reopened
                    .legacy_child_receipts("OtherLibrary", "current-child")
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                reopened
                    .legacy_child_receipts("PrimarySync", "other-child")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn legacy_relocated_receipts_reuse_only_exact_unambiguous_source() {
        for fault in [
            "none",
            "library",
            "child",
            "rendition",
            "generation",
            "bytes",
            "conflicting_source",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("state.db");
            let prior = dir.path().join("prior.jpg");
            let current = dir.path().join("current.jpg");
            std::fs::write(&prior, b"verified source").unwrap();
            std::fs::copy(&prior, &current).unwrap();
            let hash = crate::download::file::compute_sha256(&prior).await.unwrap();
            let db = SqliteStateDb::open(&database).await.unwrap();
            let record = crate::test_helpers::TestAssetRecord::new("current-child")
                .checksum("cnJycnJycnJycnJycnJycnJycnJy")
                .build();
            db.upsert_seen(&record).await.unwrap();
            crate::state::DownloadStateStore::mark_verified_download(
                &db,
                "PrimarySync",
                "current-child",
                "original",
                &prior,
                &hash,
                Some(&hash),
                false,
            )
            .await
            .unwrap();
            {
                let conn = db.acquire_lock("relocation source fault").unwrap();
                let sql = match fault {
                    "library" => "UPDATE asset_metadata_paths SET library='other'",
                    "child" => "UPDATE asset_metadata_paths SET id='other'",
                    "rendition" => "UPDATE asset_metadata_paths SET version_size='adjusted'",
                    "generation" => "UPDATE asset_metadata_paths SET provider_checksum='other'",
                    "conflicting_source" => {
                        "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum) SELECT library,id,version_size,local_path || '.conflict',provider_checksum,local_checksum,printf('%064d',0) FROM asset_metadata_paths"
                    }
                    _ => "SELECT 1",
                };
                conn.execute_batch(sql).unwrap();
            }
            if fault == "bytes" {
                std::fs::write(&current, b"different local bytes").unwrap();
            }
            let local = crate::download::file::compute_sha256(&current)
                .await
                .unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "current-child",
                "original",
                &current,
                &local,
                Some(&hash),
            )
            .await
            .unwrap();
            assert_eq!(std::fs::read(&prior).unwrap(), b"verified source");
            drop(db);
            let reopened = SqliteStateDb::open(&database).await.unwrap();
            assert_eq!(
                reopened
                    .legacy_child_receipts("PrimarySync", "current-child")
                    .await
                    .unwrap()
                    .len(),
                usize::from(fault == "none"),
                "{fault}"
            );
        }
    }

    async fn seed(
        path: &Path,
    ) -> (
        SqliteStateDb,
        LegacyPreparationSnapshot,
        Vec<LegacyFileEvidence>,
    ) {
        let db = SqliteStateDb::open(path).await.unwrap();
        let root = path.parent().unwrap();
        let media = root.join("original.jpg");
        std::fs::write(&media, b"legacy bytes").unwrap();
        let checksum = crate::download::file::compute_sha256(&media).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("master")
            .checksum("provider")
            .size(12)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("PrimarySync", "master", "original", &media, &checksum, None)
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "master", 0);
        for child in ["child-a", "child-b"] {
            db.upsert_asset_master_mapping("PrimarySync", child, "master")
                .await
                .unwrap();
        }
        let candidate = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.defer_metadata_capture_ambiguity(&candidate, 1)
                .await
                .unwrap()
        );
        db.set_metadata("sync_token:PrimarySync", "before")
            .await
            .unwrap();
        let snapshot = db
            .legacy_preparation_snapshots("PrimarySync", 10)
            .await
            .unwrap()
            .remove(0);
        let files = vec![
            LegacyFileEvidence {
                root: root.into(),
                path: media.clone(),
                sha256: Some(checksum),
                size: Some(12),
            },
            LegacyFileEvidence {
                root: root.into(),
                path: root.join("original.jpg.xmp"),
                sha256: None,
                size: None,
            },
        ];
        (db, snapshot, files)
    }
    #[tokio::test]
    async fn legacy_preservation_path_conflict_defers_only_its_family_after_reopen() {
        for claim in ["media", "metadata_sidecar"] {
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("state.db");
            let (db, snapshot, mut files) = seed(&database).await;
            let sidecar = dir.path().join("original.jpg.xmp");
            std::fs::write(&sidecar, b"legacy sidecar").unwrap();
            files[1].sha256 = Some(
                crate::download::file::compute_sha256(&sidecar)
                    .await
                    .unwrap(),
            );
            files[1].size = Some(14);
            db.upsert_seen(&crate::test_helpers::TestAssetRecord::new("child-a").build())
                .await
                .unwrap();
            if claim == "media" {
                db.mark_downloaded(
                    "PrimarySync",
                    "child-a",
                    "original",
                    &files[0].path,
                    files[0].sha256.as_deref().unwrap(),
                    None,
                )
                .await
                .unwrap();
            } else {
                db.acquire_lock("seed_child_sidecar_claim").unwrap().execute(
                    "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum) VALUES ('PrimarySync','child-a','original',?1,'provider')",
                    [sidecar.to_str().unwrap()],
                ).unwrap();
            }
            let independent = dir.path().join("independent.jpg");
            std::fs::write(&independent, b"independent bytes").unwrap();
            let checksum = crate::download::file::compute_sha256(&independent)
                .await
                .unwrap();
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new("z-independent")
                    .size(17)
                    .build(),
            )
            .await
            .unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "z-independent",
                "original",
                &independent,
                &checksum,
                None,
            )
            .await
            .unwrap();
            db.set_metadata_capture_revision_for_test("PrimarySync", "z-independent", 0);
            for child in ["independent-a", "independent-b"] {
                db.upsert_asset_master_mapping("PrimarySync", child, "z-independent")
                    .await
                    .unwrap();
            }
            let candidate = db
                .get_metadata_capture_candidates("PrimarySync", 1, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|candidate| candidate.asset_id == "z-independent")
                .unwrap();
            assert!(
                db.defer_metadata_capture_ambiguity(&candidate, 1)
                    .await
                    .unwrap()
            );
            let independent_files = vec![
                LegacyFileEvidence {
                    root: dir.path().into(),
                    path: independent.clone(),
                    sha256: Some(checksum),
                    size: Some(17),
                },
                LegacyFileEvidence {
                    root: dir.path().into(),
                    path: dir.path().join("independent.jpg.xmp"),
                    sha256: None,
                    size: None,
                },
            ];
            let durable_rows = |db: &SqliteStateDb| {
                let conn = db.acquire_lock("snapshot_conflict_state").unwrap();
                [
                    "assets",
                    "asset_metadata_paths",
                    "asset_metadata_capture_revisions",
                    "metadata_capture_retries",
                    "asset_master_mappings",
                    "legacy_master_state_owners",
                    "metadata",
                ]
                .map(|table| {
                    super::snapshot_rows(
                        &conn,
                        &format!("SELECT * FROM {table} WHERE ?1 IS NOT NULL AND ?2 IS NOT NULL"),
                        "PrimarySync",
                        "all",
                    )
                    .unwrap()
                })
            };
            let before = durable_rows(&db);
            // An expected path conflict must not hide invalid later evidence.
            for fault in ["path", "hash", "size", "duplicate", "missing_sidecar"] {
                let mut malformed = files.clone();
                match fault {
                    "path" => malformed[1].path = dir.path().join("../outside.xmp"),
                    "hash" => malformed[1].sha256 = Some("invalid".into()),
                    "size" => malformed[1].size = None,
                    "duplicate" => malformed.push(files[0].clone()),
                    "missing_sidecar" => {
                        malformed.pop();
                    }
                    _ => panic!("unexpected evidence fault"),
                }
                assert!(
                    db.prepare_legacy_preservation(&snapshot, &malformed)
                        .await
                        .is_err(),
                    "{claim}/{fault}"
                );
            }
            assert_eq!(durable_rows(&db), before);
            assert!(
                crate::state::MetadataRewriteStore::guard_primary_writer(&db, &files[0].path)
                    .await
                    .is_err()
            );
            assert!(
                crate::state::MetadataRewriteStore::guard_primary_writer(&db, &sidecar)
                    .await
                    .is_err()
            );
            drop(db);
            for cycle in 0..2 {
                let db = SqliteStateDb::open(&database).await.unwrap();
                let candidates = db
                    .legacy_preparation_snapshots("PrimarySync", 64)
                    .await
                    .unwrap();
                assert_eq!(candidates.len(), if cycle == 0 { 2 } else { 1 });
                for candidate in candidates {
                    let conflicted = candidate.asset_id == "master";
                    let evidence = if conflicted {
                        &files
                    } else {
                        &independent_files
                    };
                    assert_eq!(
                        db.prepare_legacy_preservation(&candidate, evidence)
                            .await
                            .unwrap(),
                        !conflicted
                    );
                }
                let preserved = db.legacy_preservations("PrimarySync").await.unwrap();
                assert_eq!(preserved.len(), 1);
                assert_eq!(preserved[0].asset_id, "z-independent");
                assert_eq!(preserved[0].files, independent_files);
                assert_eq!(preserved[0].active_generation, None);
                assert_eq!(durable_rows(&db), before, "{claim}/cycle={cycle}");
                assert_eq!(std::fs::read(&files[0].path).unwrap(), b"legacy bytes");
                assert_eq!(std::fs::read(&sidecar).unwrap(), b"legacy sidecar");
                assert_eq!(std::fs::read(&independent).unwrap(), b"independent bytes");
                assert!(!dir.path().join("independent.jpg.xmp").exists());
                let conn = db
                    .acquire_lock("assert_no_conflicting_preservation_paths")
                    .unwrap();
                assert_eq!(
                    conn.query_row(
                        "SELECT COUNT(*) FROM unattributed_legacy_paths WHERE asset_id='master'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                    0
                );
            }
        }
    }

    #[tokio::test]
    async fn legacy_preservation_retains_mixed_rendition_dates_without_attribution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (db, _, mut files) = seed(&path).await;
        let original_date = chrono::DateTime::from_timestamp(1_514_764_800, 0).unwrap();
        let adjusted_date = chrono::DateTime::from_timestamp(1_420_070_400, 0).unwrap();
        {
            let conn = db.acquire_lock("seed_historical_dates").unwrap();
            conn.execute(
                "UPDATE assets SET added_at=?1 WHERE library='PrimarySync' AND id='master'",
                [original_date.timestamp()],
            )
            .unwrap();
        }
        let adjusted = dir.path().join("adjusted.jpg");
        std::fs::write(&adjusted, b"adjusted historical bytes").unwrap();
        let checksum = crate::download::file::compute_sha256(&adjusted)
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("master")
            .version_size(crate::state::VersionSizeKey::Adjusted)
            .added_at(adjusted_date)
            .size(25)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "master",
            "adjusted",
            &adjusted,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "master", 0);
        files.push(LegacyFileEvidence {
            root: dir.path().into(),
            path: adjusted.clone(),
            sha256: Some(checksum),
            size: Some(25),
        });
        let snapshot = db
            .legacy_preparation_snapshots("PrimarySync", 64)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.prepare_legacy_preservation(&snapshot, &files)
                .await
                .is_err()
        );
        assert!(
            db.legacy_preservations("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        files.push(LegacyFileEvidence {
            root: dir.path().into(),
            path: dir.path().join("adjusted.jpg.xmp"),
            sha256: None,
            size: None,
        });
        assert!(
            db.prepare_legacy_preservation(&snapshot, &files)
                .await
                .unwrap()
        );
        drop(db);
        for _ in 0..2 {
            let db = SqliteStateDb::open(&path).await.unwrap();
            let preserved = db.legacy_preservations("PrimarySync").await.unwrap();
            assert_eq!(preserved.len(), 1);
            assert_eq!(preserved[0].original_evidence, snapshot.evidence);
            assert_eq!(preserved[0].active_generation, None);
            assert!(
                db.get_legacy_master_state_owners()
                    .await
                    .unwrap()
                    .is_empty()
            );
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), 2);
            for row in rows {
                let expected = match row.version_size {
                    crate::state::VersionSizeKey::Original => original_date,
                    crate::state::VersionSizeKey::Adjusted => adjusted_date,
                    _ => panic!("unexpected synthetic rendition"),
                };
                assert_eq!(row.added_at, Some(expected));
            }
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("before")
            );
            assert_eq!(
                std::fs::read(&adjusted).unwrap(),
                b"adjusted historical bytes"
            );
            assert_eq!(
                std::fs::read(dir.path().join("original.jpg")).unwrap(),
                b"legacy bytes"
            );
        }
    }

    #[tokio::test]
    async fn legacy_preservation_saved_owner_excludes_retained_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (db, snapshot, files) = seed(&path).await;
        // Model a saved claim from an older writer without asking today's
        // conservative claim gate to infer an owner for this family.
        db.acquire_lock("seed_saved_legacy_claim").unwrap().execute(
            "INSERT INTO legacy_master_state_owners VALUES ('PrimarySync','master','child-a',1)", [],
        ).unwrap();
        assert!(
            db.legacy_preparation_snapshots("PrimarySync", 64)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !db.prepare_legacy_preservation(&snapshot, &files)
                .await
                .unwrap()
        );
        drop(db);
        let db = SqliteStateDb::open(&path).await.unwrap();
        assert!(
            db.legacy_preservations("PrimarySync")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.get_legacy_master_state_owners().await.unwrap().len(), 1);
        assert_eq!(db.get_asset_master_mappings().await.unwrap().len(), 2);
        assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), 1);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        assert_eq!(
            std::fs::read(dir.path().join("original.jpg")).unwrap(),
            b"legacy bytes"
        );
    }

    #[tokio::test]
    async fn legacy_preservation_single_child_stale_receipt_keeps_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (db, snapshot, files) = seed(&path).await;
        assert!(
            db.prepare_legacy_preservation(&snapshot, &files)
                .await
                .unwrap()
        );
        let child = crate::test_helpers::TestAssetRecord::new("child-a").build();
        db.upsert_seen(&child).await.unwrap();
        // Only the SQLite compare-and-commit boundary is under test here;
        // the synthetic proof is not a claim of validated provider coverage.
        let stale = proof(&db, &snapshot).await;
        let mut changed = child.clone();
        changed.added_at = chrono::DateTime::from_timestamp(1_600_000_000, 0);
        db.upsert_seen(&changed).await.unwrap();
        assert!(
            db.commit_checkpoint_transition(transition(vec![stale]))
                .await
                .is_err()
        );
        drop(db);
        let db = SqliteStateDb::open(&path).await.unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        let preserved = db.legacy_preservations("PrimarySync").await.unwrap();
        assert_eq!(preserved[0].active_generation, None);
        assert_eq!(preserved[0].original_evidence, snapshot.evidence);
        assert!(
            db.get_legacy_master_state_owners()
                .await
                .unwrap()
                .is_empty()
        );
        let conn = db.acquire_lock("assert_no_stale_proof").unwrap();
        let proofs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM unattributed_legacy_proofs",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(proofs, 0);
    }

    async fn proof(
        db: &SqliteStateDb,
        snapshot: &LegacyPreparationSnapshot,
    ) -> LegacyActivationProof {
        LegacyActivationProof {
            library: "PrimarySync".into(),
            asset_id: "master".into(),
            expected_original: snapshot.evidence.clone(),
            provider_evidence: "synthetic-state-owner-test".into(),
            expected_metadata: vec![("enum_config_hash".into(), None)],
            expected_dependencies: db
                .legacy_dependency_evidence("PrimarySync", "master")
                .await
                .unwrap(),
            expected_active_generation: None,
            config_hash: "config".into(),
            prior_cursor: "before".into(),
            next_cursor: "after".into(),
        }
    }
    fn transition(proofs: Vec<LegacyActivationProof>) -> CheckpointTransition {
        CheckpointTransition {
            expected_metadata: Vec::new(),
            legacy_preservation_proofs: proofs,
            legacy_config_hash: Some("config".into()),
            sparse_identity_proofs: Vec::new(),
            metadata_updates: vec![("sync_token:PrimarySync".into(), "after".into())],
            metadata_deletes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn legacy_preservation_prepare_and_checkpoint_are_fenced_and_restart_safe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (db, snapshot, files) = seed(&path).await;
        assert!(
            db.prepare_legacy_preservation(&snapshot, &files)
                .await
                .unwrap()
        );
        assert!(
            db.commit_checkpoint_transition(transition(Vec::new()))
                .await
                .is_err()
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        assert_eq!(
            db.legacy_preservations("PrimarySync").await.unwrap()[0].active_generation,
            None
        );
        db.touch_last_seen_many("PrimarySync", &["master"])
            .await
            .unwrap();
        assert_eq!(
            db.legacy_preservations("PrimarySync").await.unwrap()[0].original_evidence,
            snapshot.evidence
        );
        let activation = proof(&db, &snapshot).await;
        db.commit_checkpoint_transition(transition(vec![activation.clone()]))
            .await
            .unwrap();
        drop(db);
        let db = SqliteStateDb::open(&path).await.unwrap();
        let retained = db
            .legacy_preservations("PrimarySync")
            .await
            .unwrap()
            .remove(0);
        assert_eq!(retained.original_evidence, snapshot.evidence);
        assert_eq!(retained.files, files);
        assert_eq!(retained.active_generation, Some(1));
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("after")
        );
        assert_eq!(
            std::fs::read(dir.path().join("original.jpg")).unwrap(),
            b"legacy bytes"
        );
        assert!(
            db.commit_checkpoint_transition(transition(vec![activation]))
                .await
                .is_err()
        );
        // An unchanged subsequent cursor uses retained coverage, without a new
        // preservation receipt. Current-work proof remains the caller's duty.
        db.commit_checkpoint_transition(transition(Vec::new()))
            .await
            .unwrap();
        assert_eq!(
            db.legacy_preservations("PrimarySync").await.unwrap()[0].active_generation,
            Some(1)
        );
        assert!(
            db.reactivate_legacy_preservation("PrimarySync", "master", Some(0))
                .await
                .is_err()
        );
        assert_eq!(
            db.legacy_preservations("PrimarySync").await.unwrap()[0].active_generation,
            Some(1)
        );
        db.reactivate_legacy_preservation("PrimarySync", "master", Some(1))
            .await
            .unwrap();
        assert!(
            db.commit_checkpoint_transition(transition(Vec::new()))
                .await
                .is_err()
        );
        assert_eq!(
            db.legacy_preservations("PrimarySync").await.unwrap()[0].original_evidence,
            snapshot.evidence
        );
    }

    #[tokio::test]
    async fn legacy_preservation_checkpoint_failure_rolls_back_activation() {
        for failure in ["receipt", "cursor"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let (db, snapshot, files) = seed(&path).await;
            assert!(
                db.prepare_legacy_preservation(&snapshot, &files)
                    .await
                    .unwrap()
            );
            let sql = if failure == "receipt" {
                "CREATE TRIGGER fail_proof BEFORE INSERT ON unattributed_legacy_proofs BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;"
            } else {
                "CREATE TRIGGER fail_cursor BEFORE UPDATE ON metadata WHEN OLD.key='sync_token:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected cursor failure'); END;"
            };
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
            assert!(
                db.commit_checkpoint_transition(transition(vec![proof(&db, &snapshot).await]))
                    .await
                    .is_err()
            );
            drop(db);
            let db = SqliteStateDb::open(&path).await.unwrap();
            assert_eq!(
                db.legacy_preservations("PrimarySync").await.unwrap()[0].active_generation,
                None
            );
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("before")
            );
            assert_eq!(
                rusqlite::Connection::open(&path)
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM unattributed_legacy_proofs", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }

    #[tokio::test]
    async fn legacy_preservation_rejects_stale_snapshot_family_and_cursor() {
        for change in [
            "UPDATE metadata_capture_retries SET generation=generation+1",
            "UPDATE assets SET added_at=99",
            "INSERT INTO asset_master_mappings VALUES ('PrimarySync','third','master',0)",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let (db, snapshot, files) = seed(&path).await;
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(change)
                .unwrap();
            assert!(
                !db.prepare_legacy_preservation(&snapshot, &files)
                    .await
                    .unwrap()
            );
            assert!(
                db.legacy_preservations("PrimarySync")
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        for change in [
            "INSERT INTO asset_master_mappings VALUES ('PrimarySync','third','master',0)",
            "UPDATE metadata SET value='concurrent' WHERE key='sync_token:PrimarySync'",
            "INSERT INTO metadata(key,value) VALUES ('enum_config_hash','concurrent')",
            "INSERT INTO asset_master_mappings VALUES ('PrimarySync','master','different-master',0)",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let (db, snapshot, files) = seed(&path).await;
            assert!(
                db.prepare_legacy_preservation(&snapshot, &files)
                    .await
                    .unwrap()
            );
            let activation = proof(&db, &snapshot).await;
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(change)
                .unwrap();
            assert!(
                db.commit_checkpoint_transition(transition(vec![activation]))
                    .await
                    .is_err()
            );
            assert_eq!(
                db.legacy_preservations("PrimarySync").await.unwrap()[0].active_generation,
                None
            );
        }
    }
    #[tokio::test]
    async fn legacy_preservation_staged_checkpoint_promotion_is_atomic_and_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (db, snapshot, files) = seed(&path).await;
        assert!(
            db.prepare_legacy_preservation(&snapshot, &files)
                .await
                .unwrap()
        );
        let protected =
            crate::download::legacy_preservation::protected_replacement_paths(Some(&db))
                .await
                .unwrap();
        assert_eq!(protected.len(), 2);
        assert!(files.iter().all(|file| protected.contains(&file.path)));
        assert!(
            db.set_metadata("sync_token:PrimarySync", "unsafe")
                .await
                .is_err()
        );
        assert!(
            db.set_metadata("pending_sync_token:config:PrimarySync", "unsafe")
                .await
                .is_err()
        );
        db.set_metadata("sync_token:OtherLibrary", "independent")
            .await
            .unwrap();
        let mut staged = transition(vec![proof(&db, &snapshot).await]);
        staged.metadata_updates = vec![(
            "pending_sync_token:config:PrimarySync".into(),
            "after".into(),
        )];
        db.commit_checkpoint_transition(staged).await.unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        let mut promotion = transition(Vec::new());
        promotion.metadata_deletes = vec!["pending_sync_token:config:PrimarySync".into()];
        promotion.legacy_config_hash = Some("changed".into());
        assert!(db.commit_checkpoint_transition(promotion).await.is_err());
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before")
        );
        assert_eq!(
            db.get_metadata("pending_sync_token:config:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("after")
        );
        let mut promotion = transition(Vec::new());
        promotion.metadata_deletes = vec!["pending_sync_token:config:PrimarySync".into()];
        db.commit_checkpoint_transition(promotion).await.unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("after")
        );
        assert!(
            db.get_metadata("pending_sync_token:config:PrimarySync")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.get_metadata("sync_token:OtherLibrary")
                .await
                .unwrap()
                .as_deref(),
            Some("independent")
        );
        assert_eq!(
            db.legacy_preservations("PrimarySync").await.unwrap()[0].active_generation,
            Some(1)
        );
    }
}
