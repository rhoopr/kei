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
    // Layout keys use case and AM/PM collision equivalence without lossy native
    // paths. Read-only protection still uses the exact UTF-8 local_path spelling;
    // native byte paths use the encoded SelectionPath key directly.
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

pub(super) fn path_is_managed(
    conn: &Connection,
    path: &std::path::Path,
) -> Result<bool, StateError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?1)",
        [path_key(path)?],
        |r| r.get(0),
    )?)
}

pub(super) fn guard_writer(conn: &Connection, path: &std::path::Path) -> Result<(), StateError> {
    if path_is_managed(conn, path)? {
        return Err(invalid(
            "managed primary slot requires its layout publication owner",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadonlyMarker {
    Marked,
    Unchanged,
    Missing,
    Conflict,
    Unsupported,
}

/// Persist the existing read-only preservation marker atomically with one
/// verified reuse settlement. Only an exact compatibility row may carry it.
pub(super) fn persist_readonly_marker(
    conn: &Connection,
    library: &str,
    asset: &str,
    version: &str,
    native_path: &std::path::Path,
    provider_checksum: &str,
    local_checksum: &str,
    source_checksum: &str,
) -> Result<ReadonlyMarker, StateError> {
    let Ok(normalized) = crate::fs_util::absolute_lexical(native_path) else {
        return Ok(ReadonlyMarker::Unsupported);
    };
    let marker = path_key(&normalized)?;
    // A native path and a real UTF-8 sibling can share the same lossy TEXT
    // spelling. If that sibling exists, the locator is ambiguous and no row
    // may be marked or settled from the native path alone.
    if normalized.to_str().is_none() {
        let lossy = std::path::PathBuf::from(normalized.to_string_lossy().as_ref());
        if lossy != normalized {
            match std::fs::symlink_metadata(&lossy) {
                Ok(_) => return Ok(ReadonlyMarker::Missing),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Ok(ReadonlyMarker::Missing),
            }
        }
    }
    // UTF-8 paths use the exact TEXT spelling. Native byte paths cannot be
    // represented losslessly in local_path, so require one unique compatibility
    // row for the same library/id/version/provider/local evidence.
    let existing: Option<(String, Option<String>, Option<String>)> = match normalized.to_str() {
        Some(local_path) => conn
            .query_row(
                "SELECT local_path,source_checksum,preserved_path_key FROM asset_metadata_paths
                 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4
                   AND provider_checksum=?5 AND local_checksum=?6",
                params![
                    library,
                    asset,
                    version,
                    local_path,
                    provider_checksum,
                    local_checksum
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?,
        None => {
            let rows: Vec<(String, Option<String>, Option<String>)> = conn
                .prepare(
                    "SELECT local_path,source_checksum,preserved_path_key FROM asset_metadata_paths
                     WHERE library=?1 AND id=?2 AND version_size=?3
                       AND provider_checksum=?4 AND local_checksum=?5 AND local_path=?6",
                )?
                .query_map(
                    params![
                        library,
                        asset,
                        version,
                        provider_checksum,
                        local_checksum,
                        normalized.to_string_lossy()
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?
                .collect::<Result<_, _>>()?;
            if rows.len() == 1 {
                rows.into_iter().next()
            } else {
                None
            }
        }
    };
    let Some((local_path, stored_source, stored_marker)) = existing else {
        return Ok(ReadonlyMarker::Missing);
    };
    match stored_source.as_deref() {
        Some(source) if source != source_checksum => return Ok(ReadonlyMarker::Conflict),
        Some(_) => {}
        None => {
            let changed = conn.execute(
                "UPDATE asset_metadata_paths SET source_checksum=?7
                 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4
                   AND provider_checksum=?5 AND local_checksum=?6 AND source_checksum IS NULL",
                params![
                    library,
                    asset,
                    version,
                    local_path,
                    provider_checksum,
                    local_checksum,
                    source_checksum
                ],
            )?;
            if changed != 1 {
                return Ok(ReadonlyMarker::Conflict);
            }
        }
    }
    match stored_marker {
        None => {
            let changed = conn.execute(
                "UPDATE asset_metadata_paths SET preserved_path_key=?7
                 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4
                   AND provider_checksum=?5 AND local_checksum=?6 AND source_checksum=?8",
                params![
                    library,
                    asset,
                    version,
                    local_path,
                    provider_checksum,
                    local_checksum,
                    marker,
                    source_checksum
                ],
            )?;
            if changed != 1 {
                return Ok(ReadonlyMarker::Missing);
            }
            Ok(ReadonlyMarker::Marked)
        }
        Some(current) if current == marker => Ok(ReadonlyMarker::Unchanged),
        Some(_) => Ok(ReadonlyMarker::Conflict),
    }
}

fn readonly_local_paths(path: &std::path::Path) -> (Option<String>, Option<String>) {
    let Ok(path) = crate::fs_util::absolute_lexical(path) else {
        return (None, None);
    };
    let Some(requested) = path.to_str().map(str::to_owned) else {
        return (None, None);
    };
    let is_sidecar = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xmp"));
    if is_sidecar {
        (
            path.with_extension("").to_str().map(str::to_owned),
            Some(requested),
        )
    } else {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".xmp");
        (
            Some(requested),
            std::path::Path::new(&sidecar).to_str().map(str::to_owned),
        )
    }
}

fn protected_readonly_pair(path: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let is_sidecar = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xmp"));
    if is_sidecar {
        (path.with_extension(""), path.to_path_buf())
    } else {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".xmp");
        (path.to_path_buf(), std::path::PathBuf::from(sidecar))
    }
}

/// A persisted marker protects the exact recorded media path and its exact
/// sidecar spelling. Native byte paths use their encoded marker key because the
/// TEXT local_path is only a lossy locator. A marker that no longer matches its
/// recorded UTF-8 path keeps that recorded path protected and unresolved.
pub(super) fn path_is_readonly(
    conn: &Connection,
    path: &std::path::Path,
) -> Result<bool, StateError> {
    let normalized = crate::fs_util::absolute_lexical(path)
        .map_err(|_error| invalid("invalid read-only path"))?;
    if normalized.to_str().is_some() {
        let (media, sidecar) = readonly_local_paths(&normalized);
        if media.is_none() && sidecar.is_none() {
            return Ok(false);
        }
        let mut stmt = conn.prepare(
            "SELECT local_path,preserved_path_key FROM asset_metadata_paths
             WHERE local_path IN (?1,?2) AND preserved_path_key IS NOT NULL",
        )?;
        let rows = stmt
            .query_map(params![media, sidecar], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (recorded, marker) in rows {
            if let Ok(selection) = serde_json::from_str::<SelectionPath>(&marker)
                && !matches!(selection, SelectionPath::Utf8(_))
            {
                // A native marker's lossy local_path text is not an exact path.
                continue;
            }
            if path_key(std::path::Path::new(&recorded))? != marker {
                tracing::warn!(
                    "Read-only preservation marker does not match its recorded path; keeping that path protected"
                );
            }
            return Ok(true);
        }
        return Ok(false);
    }
    // Native byte paths are represented losslessly by their encoded
    // SelectionPath key. local_path is only a lossy TEXT locator.
    let (media, sidecar) = readonly_native_keys(&normalized)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM asset_metadata_paths
         WHERE preserved_path_key IN (?1,?2))",
        params![media, sidecar],
        |row| row.get(0),
    )?)
}

fn readonly_native_keys(path: &std::path::Path) -> Result<(String, String), StateError> {
    let is_sidecar = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xmp"));
    let (media, sidecar) = if is_sidecar {
        (path.with_extension(""), path.to_path_buf())
    } else {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".xmp");
        (path.to_path_buf(), std::path::PathBuf::from(sidecar))
    };
    Ok((path_key(&media)?, path_key(&sidecar)?))
}

pub(super) fn protected_readonly_paths(
    conn: &Connection,
) -> Result<Vec<std::path::PathBuf>, StateError> {
    let mut stmt = conn.prepare(
        "SELECT local_path,preserved_path_key FROM asset_metadata_paths
         WHERE preserved_path_key IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut paths = Vec::new();
    for (recorded, marker) in rows {
        // A native marker is already the lossless path representation. Do not
        // add the lossy recorded TEXT spelling as an independent path.
        if let Ok(selection) = serde_json::from_str::<SelectionPath>(&marker)
            && !matches!(selection, SelectionPath::Utf8(_))
        {
            let (media, sidecar) = protected_readonly_pair(&selection.to_path());
            paths.push(media);
            paths.push(sidecar);
            continue;
        }
        let native = std::path::PathBuf::from(&recorded);
        if path_key(&native)? != marker {
            tracing::warn!(
                "Read-only preservation marker does not match its recorded path; retaining that recorded path"
            );
        }
        let (media, sidecar) = protected_readonly_pair(&native);
        paths.push(media);
        paths.push(sidecar);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

// A journal may publish old prepared bytes, but must never roll newer provider
// observations back through its stored AssetRecord. This check runs inside the
// phase/finalization transaction, independently of orchestration preflight.
fn guard_recorded_catalogue(conn: &Connection, op: &LayoutOperation) -> Result<(), StateError> {
    for member in &op.members {
        if let Some(expected) = &member.record {
            let current = conn
                .query_row(
                    &format!(
                        "SELECT {} FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
                        super::rows::ASSET_COLUMNS
                    ),
                    params![
                        expected.library,
                        expected.id,
                        expected.version_size.as_str()
                    ],
                    super::rows::row_to_asset_record,
                )
                .optional()?;
            let Some(current) = current else { continue };
            let stored_metadata = |record: &AssetRecord| -> Result<serde_json::Value, StateError> {
                let mut metadata = (*record.metadata).clone();
                metadata.metadata_hash = Some(
                    metadata
                        .metadata_hash
                        .clone()
                        .unwrap_or_else(|| metadata.compute_hash()),
                );
                if metadata.source.is_none() {
                    metadata.source = Some("icloud".into());
                }
                metadata.modified_at = metadata
                    .modified_at
                    .and_then(|date| chrono::DateTime::from_timestamp(date.timestamp(), 0));
                metadata.deleted_at = metadata
                    .deleted_at
                    .and_then(|date| chrono::DateTime::from_timestamp(date.timestamp(), 0));
                serde_json::to_value(metadata)
                    .map_err(|_error| invalid("invalid journal metadata snapshot"))
            };
            if current.checksum != expected.checksum
                || current.filename != expected.filename
                || current.size_bytes != expected.size_bytes
                || current.media_type != expected.media_type
                || super::rows::encode_asset_date(current.created_at).to_bits()
                    != super::rows::encode_asset_date(expected.created_at).to_bits()
                || current
                    .added_at
                    .map(|date| super::rows::encode_asset_date(date).to_bits())
                    != expected
                        .added_at
                        .map(|date| super::rows::encode_asset_date(date).to_bits())
                || stored_metadata(&current)? != stored_metadata(expected)?
            {
                return Err(invalid(
                    "recorded layout differs from current provider observation",
                ));
            }
        } else if let Some(source) = &member.archive_source {
            // Archive-only finalization does not replace provider metadata, but
            // can move its canonical path. Permit only the exact recorded source.
            let current: Option<(String, Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT checksum,local_path,local_checksum FROM assets
                 WHERE library=?1 AND id=?2 AND version_size=?3",
                    params![op.library, op.child, source.version],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((checksum, path, local)) = current
                && checksum == source.provider_checksum
            {
                let source_path = source.path.to_path();
                if source_path.to_str().is_none()
                    || path.as_deref() != source_path.to_str()
                    || local.as_deref()
                        != Some(
                            data_encoding::HEXLOWER
                                .encode(&source.fingerprint.sha256)
                                .as_str(),
                        )
                {
                    return Err(invalid(
                        "recorded archive source differs from current canonical path",
                    ));
                }
            }
        }
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
            guard_recorded_catalogue(&tx, &op)?;
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
            guard_recorded_catalogue(&tx, &op)?;
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
        LayoutBinding, LayoutFile, LayoutFingerprint, LayoutMember, LayoutOperation,
        ReadonlyMarker, decode, encode, path_is_readonly, path_key, persist_readonly_marker,
        protected_readonly_paths,
    };
    #[cfg(target_os = "linux")]
    use crate::state::MetadataRewriteStore;
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
            latitude: Some(51.248178375505404),
            longitude: Some(-93.31137037688033),
            altitude: Some(97.45365320034685),
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
    async fn primary_layout_legacy_float_journal_reopens_saves_and_commits_exactly() {
        for text in ["1.9333333333333333", "1.9333333333333331"] {
            let root = tempfile::tempdir().unwrap();
            let db_path = root.path().join("state.db");
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            let seed = fixture(root.path());
            db.begin_primary_layout(seed.clone()).await.unwrap();
            // Literal evidence uses the pre-fix writer's JSON format, without
            // passing the reported decimal through the decoder under test.
            let legacy = String::from_utf8(encode(&seed.members[0]).unwrap())
                .unwrap()
                .replace(
                    "\"duration_secs\":2.5",
                    &format!("\"duration_secs\":{text}"),
                );
            db.with_conn("seed_legacy_layout", move |conn| {
                conn.execute(
                    "UPDATE primary_layout_members SET evidence=?1",
                    [legacy.into_bytes()],
                )?;
                Ok(())
            })
            .await
            .unwrap();
            drop(db);
            let duration: f64 = text.parse().unwrap();
            let mut op = seed;
            Arc::make_mut(&mut op.members[0].record.as_mut().unwrap().metadata).duration_secs =
                Some(duration);
            let mut file = op.members[0].old.clone().unwrap();
            file.version = "adjusted".into();
            file.provider_checksum = "new-provider-checksum".into();
            for phase in ["planned", "prepared", "preserved", "publishing"] {
                let db = SqliteStateDb::open(&db_path).await.unwrap();
                let mut loaded = db
                    .primary_layout_operations(op.source_library.clone())
                    .await
                    .unwrap()
                    .pop()
                    .unwrap();
                db.save_primary_layout(loaded.clone()).await.unwrap();
                let metadata = &loaded.members[0].record.as_ref().unwrap().metadata;
                assert_eq!(
                    metadata.duration_secs.unwrap().to_bits(),
                    duration.to_bits()
                );
                assert_eq!(encode(&loaded).unwrap(), encode(&op).unwrap());
                let mut changed = loaded.clone();
                Arc::make_mut(&mut changed.members[0].record.as_mut().unwrap().metadata)
                    .duration_secs = Some(f64::from_bits(duration.to_bits() ^ 1));
                assert!(db.save_primary_layout(changed).await.is_err());
                loaded.phase = phase.into();
                if phase != "planned" {
                    loaded.members[0].prepared = Some(file.clone());
                }
                if matches!(phase, "preserved" | "publishing") {
                    loaded.members[0].preserved = loaded.members[0].old.clone();
                }
                db.save_primary_layout(loaded.clone()).await.unwrap();
                op = loaded;
            }
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            op = db
                .primary_layout_operations(op.source_library.clone())
                .await
                .unwrap()
                .pop()
                .unwrap();
            op.members[0].installed = Some(file);
            db.save_primary_layout(op.clone()).await.unwrap();
            drop(db);
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            op = db
                .primary_layout_operations(op.source_library.clone())
                .await
                .unwrap()
                .pop()
                .unwrap();
            db.commit_primary_layout(op.clone()).await.unwrap();
            assert!(
                db.primary_layout_operations(op.source_library.clone())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                db.primary_layout_binding(op.family)
                    .await
                    .unwrap()
                    .unwrap()
                    .generation,
                1
            );
            drop(db);
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let persisted: f64 = conn
                .query_row("SELECT duration_secs FROM assets", [], |row| row.get(0))
                .unwrap();
            assert_eq!(persisted.to_bits(), duration.to_bits());
        }
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
        for field in [
            "child",
            "destination",
            "stage",
            "title",
            "hash",
            "old_bytes",
            "sidecar",
        ] {
            let mut wrong = op.clone();
            let member = &mut wrong.members[0];
            match field {
                "child" => member.record.as_mut().unwrap().id = "other-child".into(),
                "destination" => {
                    member.destination = Some(SelectionPath::from_path(&root.path().join("other")))
                }
                "stage" => {
                    member.stage = Some(SelectionPath::from_path(&root.path().join("other")))
                }
                "title" => {
                    Arc::make_mut(&mut member.record.as_mut().unwrap().metadata).title =
                        Some("changed".into())
                }
                "hash" => {
                    Arc::make_mut(&mut member.record.as_mut().unwrap().metadata).metadata_hash =
                        Some("changed".into())
                }
                "old_bytes" => member.old.as_mut().unwrap().fingerprint.sha256 = [8; 32],
                "sidecar" => member.old.as_mut().unwrap().sidecar.as_mut().unwrap().size += 1,
                _ => panic!("unexpected journal mutation: {field}"),
            }
            assert!(db.save_primary_layout(wrong).await.is_err(), "{field}");
        }
        let old = op.members[0].old.clone().unwrap();
        op.members[0].prepared = Some(old.clone());
        op.members[0].preserved = Some(old.clone());
        op.members[0].installed = Some(old.clone());
        db.save_primary_layout(op.clone()).await.unwrap();
        for field in ["preserved", "installed"] {
            let mut wrong = op.clone();
            let member = &mut wrong.members[0];
            let file = if field == "preserved" {
                &mut member.preserved
            } else {
                &mut member.installed
            };
            file.as_mut().unwrap().fingerprint.sha256 = [9; 32];
            assert!(db.save_primary_layout(wrong).await.is_err(), "{field}");
        }
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

    #[test]
    fn readonly_marker_is_exact_and_conflicts_are_refused() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let conn = db.acquire_lock("readonly marker test").unwrap();
        let native = std::path::Path::new("/photos/photo.jpg");
        let marker = path_key(native).unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_paths
             (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum)
             VALUES ('PrimarySync','child','original',?1,'provider','local',NULL)",
            [native.to_str().unwrap()],
        )
        .unwrap();
        assert_eq!(
            persist_readonly_marker(
                &conn,
                "PrimarySync",
                "child",
                "original",
                native,
                "provider",
                "local",
                "source",
            )
            .unwrap(),
            ReadonlyMarker::Marked
        );
        assert_eq!(
            persist_readonly_marker(
                &conn,
                "PrimarySync",
                "child",
                "original",
                native,
                "provider",
                "local",
                "source",
            )
            .unwrap(),
            ReadonlyMarker::Unchanged
        );
        assert!(path_is_readonly(&conn, native).unwrap());
        assert!(path_is_readonly(&conn, std::path::Path::new("/photos/photo.jpg.xmp")).unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT source_checksum FROM asset_metadata_paths WHERE id='child'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            "source"
        );
        conn.execute(
            "UPDATE asset_metadata_paths SET preserved_path_key='conflict' WHERE id='child'",
            [],
        )
        .unwrap();
        assert_eq!(
            persist_readonly_marker(
                &conn,
                "PrimarySync",
                "child",
                "original",
                native,
                "provider",
                "local",
                "source",
            )
            .unwrap(),
            ReadonlyMarker::Conflict
        );
        conn.execute(
            "UPDATE asset_metadata_paths SET local_path='/photos/other.jpg', preserved_path_key=?1 WHERE id='child'",
            [&marker],
        )
        .unwrap();
        assert!(path_is_readonly(&conn, std::path::Path::new("/photos/other.jpg")).unwrap());
        assert_eq!(
            persist_readonly_marker(
                &conn,
                "PrimarySync",
                "child",
                "original",
                native,
                "provider",
                "local",
                "source",
            )
            .unwrap(),
            ReadonlyMarker::Missing
        );
        assert_eq!(marker, path_key(native).unwrap());
    }

    #[test]
    fn readonly_marker_does_not_equate_case_distinct_paths() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let conn = db.acquire_lock("case-distinct marker test").unwrap();
        let lower = std::path::Path::new("/photos/photo.jpg");
        let upper = std::path::Path::new("/photos/photo.JPG");
        conn.execute(
            "INSERT INTO asset_metadata_paths
             (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,preserved_path_key)
             VALUES ('PrimarySync','child','original',?1,'provider','local','source',?2)",
            rusqlite::params![lower.to_str().unwrap(), path_key(lower).unwrap()],
        )
        .unwrap();
        assert!(path_is_readonly(&conn, lower).unwrap());
        assert!(!path_is_readonly(&conn, upper).unwrap());
        assert_eq!(
            persist_readonly_marker(
                &conn,
                "PrimarySync",
                "child",
                "original",
                upper,
                "provider",
                "local",
                "source",
            )
            .unwrap(),
            ReadonlyMarker::Missing
        );
    }

    #[test]
    fn protected_readonly_paths_does_not_double_suffix_sidecars() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let conn = db.acquire_lock("sidecar protection test").unwrap();
        let sidecar = std::path::Path::new("/photos/photo.jpg.xmp");
        conn.execute(
            "INSERT INTO asset_metadata_paths
             (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,preserved_path_key)
             VALUES ('PrimarySync','child','original',?1,'provider','local','source',?2)",
            rusqlite::params![sidecar.to_str().unwrap(), path_key(sidecar).unwrap()],
        )
        .unwrap();
        let protected = protected_readonly_paths(&conn).unwrap();
        assert!(
            protected
                .iter()
                .any(|path| path == std::path::Path::new("/photos/photo.jpg"))
        );
        assert!(protected.iter().any(|path| path == sidecar));
        assert!(
            !protected
                .iter()
                .any(|path| path == std::path::Path::new("/photos/photo.jpg.xmp.xmp"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn readonly_marker_encodes_non_utf8_native_paths() {
        use std::os::unix::ffi::OsStringExt;
        let db = SqliteStateDb::open_in_memory().unwrap();
        let conn = db.acquire_lock("non-utf8 marker test").unwrap();
        let native = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b'p', 0xff, b'.', b'j', b'p', b'g',
        ]));
        conn.execute(
            "INSERT INTO asset_metadata_paths
             (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum)
             VALUES ('PrimarySync','child','original',?1,'provider','local',NULL)",
            [native.to_string_lossy()],
        )
        .unwrap();
        assert_eq!(
            persist_readonly_marker(
                &conn,
                "PrimarySync",
                "child",
                "original",
                &native,
                "provider",
                "local",
                "source",
            )
            .unwrap(),
            ReadonlyMarker::Marked
        );
        assert!(path_is_readonly(&conn, &native).unwrap());
        let lossy = std::path::PathBuf::from(native.to_string_lossy().as_ref());
        assert!(!path_is_readonly(&conn, &lossy).unwrap());
        assert!(
            protected_readonly_paths(&conn)
                .unwrap()
                .iter()
                .any(|path| path == &native)
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_marker_does_not_attach_to_a_same_evidence_sibling() {
        use std::os::unix::ffi::OsStringExt;
        let db = SqliteStateDb::open_in_memory().unwrap();
        let conn = db.acquire_lock("non-utf8 sibling test").unwrap();
        let native = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b'p', 0xff, b'.', b'j', b'p', b'g',
        ]));
        conn.execute(
            "INSERT INTO asset_metadata_paths
             (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum)
             VALUES ('PrimarySync','child','original','/photos/sibling.jpg','provider','local',NULL)",
            [],
        )
        .unwrap();
        assert_eq!(
            persist_readonly_marker(
                &conn,
                "PrimarySync",
                "child",
                "original",
                &native,
                "provider",
                "local",
                "source",
            )
            .unwrap(),
            ReadonlyMarker::Missing
        );
        let marker: Option<String> = conn
            .query_row(
                "SELECT preserved_path_key FROM asset_metadata_paths WHERE local_path='/photos/sibling.jpg'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(marker.is_none());
    }
    #[cfg(all(unix, target_os = "linux"))]
    #[tokio::test]
    async fn native_and_utf8_siblings_with_identical_lossy_text_do_not_share_markers() {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let native = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"native-\xff.jpg".to_vec()));
        let lossy = std::path::PathBuf::from(native.to_string_lossy().as_ref());
        assert_ne!(native, lossy);
        std::fs::write(&native, b"same bytes").unwrap();
        std::fs::write(&lossy, b"same bytes").unwrap();
        let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(b"same bytes"));
        let native_text = native.to_string_lossy().into_owned();

        let database = dir.path().join("state.db");
        let db = SqliteStateDb::open_owned(
            &database,
            &crate::state::db::account::AccountOwner::authenticated(
                "collision@example.invalid",
                "com",
                &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"native-collision"}}))
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        {
            let conn = db.acquire_lock("seed colliding utf8 row").unwrap();
            conn.execute(
                "INSERT INTO asset_metadata_paths
                 (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum)
                 VALUES ('PrimarySync','utf8','original',?1,'provider',?2,?2)",
                rusqlite::params![native_text, checksum],
            )
            .unwrap();
            assert_eq!(
                persist_readonly_marker(
                    &conn,
                    "PrimarySync",
                    "utf8",
                    "original",
                    &native,
                    "provider",
                    &checksum,
                    &checksum,
                )
                .unwrap(),
                ReadonlyMarker::Missing,
                "a colliding UTF-8 sibling makes the native locator ambiguous"
            );
            let marker: Option<String> = conn
                .query_row(
                    "SELECT preserved_path_key FROM asset_metadata_paths WHERE id='utf8'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(marker.is_none());
        }
        assert!(db.guard_primary_writer(&native).await.is_ok());
        assert!(db.guard_primary_writer(&lossy).await.is_ok());
        assert!(
            !path_is_readonly(&db.acquire_lock("native unprotected").unwrap(), &native).unwrap()
        );
        assert!(!path_is_readonly(&db.acquire_lock("lossy unprotected").unwrap(), &lossy).unwrap());

        // A marker attached to the UTF-8 sibling must not protect the native path.
        let utf8_marker = path_key(&lossy).unwrap();
        db.acquire_lock("mark utf8 sibling")
            .unwrap()
            .execute(
                "UPDATE asset_metadata_paths SET preserved_path_key=?1 WHERE id='utf8'",
                [&utf8_marker],
            )
            .unwrap();
        assert!(db.guard_primary_writer(&native).await.is_ok());
        assert!(db.guard_primary_writer(&lossy).await.is_err());

        // A native marker key must not protect the lossy UTF-8 sibling.
        {
            let conn = db.acquire_lock("seed native row").unwrap();
            conn.execute(
                "INSERT INTO asset_metadata_paths
                 (library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,preserved_path_key)
                 VALUES ('PrimarySync','native','original',?1,'provider',?2,?2,?3)",
                rusqlite::params![native_text, checksum, path_key(&native).unwrap()],
            )
            .unwrap();
            conn.execute(
                "UPDATE asset_metadata_paths SET preserved_path_key=NULL WHERE id='utf8'",
                [],
            )
            .unwrap();
        }
        assert!(db.guard_primary_writer(&native).await.is_err());
        assert!(db.guard_primary_writer(&lossy).await.is_ok());
        assert!(
            !path_is_readonly(
                &db.acquire_lock("lossy not native-protected").unwrap(),
                &lossy
            )
            .unwrap()
        );

        drop(db);
        let reopened = SqliteStateDb::open_owned(
            &database,
            &crate::state::db::account::AccountOwner::authenticated(
                "collision@example.invalid",
                "com",
                &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"native-collision"}}))
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(reopened.guard_primary_writer(&native).await.is_err());
        assert!(reopened.guard_primary_writer(&lossy).await.is_ok());
        assert_eq!(
            persist_readonly_marker(
                &reopened.acquire_lock("reopened collision refusal").unwrap(),
                "PrimarySync",
                "utf8",
                "original",
                &native,
                "provider",
                &checksum,
                &checksum,
            )
            .unwrap(),
            ReadonlyMarker::Missing
        );
    }
}
