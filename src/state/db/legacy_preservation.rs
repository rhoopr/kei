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

// Preserve SQLite values without lossy float/date formatting. Schema-versioned
// column names and values are part of the receipt, not executable SQL input.
fn snapshot_rows(
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

impl SqliteStateDb {
    /// Return only independently downloaded, finalized current-child receipts.
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
        self.with_conn("legacy_child_receipts",move |conn| {
            let ready:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2 AND revision>=?3) AND NOT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND (status!='downloaded' OR is_deleted!=0 OR metadata_hash IS NULL OR metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL)) AND NOT EXISTS(SELECT 1 FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND (metadata_write_failed_at IS NOT NULL OR capture_repair_metadata_hash IS NOT NULL OR capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL))",params![library,id,crate::state::METADATA_CAPTURE_REVISION],|r|r.get(0))?;
            if !ready { return Ok(Vec::new()); }
            let mut stmt=conn.prepare(&format!("SELECT {} FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size",super::rows::ASSET_COLUMNS))?;
            Ok(stmt.query_map(params![library,id],super::rows::row_to_asset_record)?.collect::<Result<Vec<_>,_>>()?)
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
                if !keys.insert(key.clone()) { return Err(invalid()); }
                if foreign_paths.contains(&key) { return Err(invalid()); }
            }
            // Every media path and its exact sidecar name must be represented.
            let required: std::collections::HashSet<_>=expected.paths.iter().flat_map(|path| {
                let mut sidecar = path.as_os_str().to_os_string();
                sidecar.push(".xmp");
                [path.clone(), PathBuf::from(sidecar)]
            }).collect();
            if files.len()!=required.len() || files.iter().any(|file| !required.contains(&file.path))
                || expected.paths.iter().any(|path| !files.iter().any(|file| &file.path==path && file.sha256.is_some())) { return Err(invalid()); }
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
