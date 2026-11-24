//! Album-directory transitions in the existing layout journal. No file receipt
//! or provider manifest is rewritten when its directory moves.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{LayoutBinding, LayoutFingerprint, decode, encode, invalid, path_key};
use crate::fs_util::FileIdentity;
use crate::state::db::rows::decode_asset_date;
use crate::state::db::{SqliteStateDb, account, provider_selection::SelectionPath};
use crate::state::error::StateError;

pub(crate) const POLICY: &str = "album-directory-v1";

fn is_false(value: &bool) -> bool {
    !*value
}

pub(super) fn fences_path(conn: &Connection, path: &std::path::Path) -> Result<bool, StateError> {
    let path = crate::fs_util::absolute_lexical(path)
        .map_err(|_error| invalid("invalid album writer path"))?;
    let mut statement = conn.prepare(
        "SELECT operation FROM primary_layout_operations WHERE policy=?1 AND phase<>'cancelled'",
    )?;
    for row in statement.query_map([POLICY], |row| row.get::<_, String>(0))? {
        let (op, phase) = load_operation(conn, &row?)?;
        let source = if op.old.unclaimed || op.old.additional_projection.is_some() {
            let mut members =
                conn.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1")?;
            let mut owned = false;
            for row in members.query_map([&op.operation], |row| row.get::<_, Vec<u8>>(0))? {
                let file: AlbumDirectoryFile = decode(&row?)?;
                owned |= file.old.to_path() == path && !other_selected_owner(conn, &op, &file)?;
            }
            owned || (!op.old.unclaimed && path.starts_with(op.old.path.to_path()))
        } else {
            path.starts_with(op.old.path.to_path())
        };
        if source
            || (phase != "committed"
                && (path.starts_with(op.next.path.to_path())
                    || op
                        .directories
                        .iter()
                        .any(|route| path.starts_with(route.next.to_path()))))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AlbumDirectoryBinding {
    pub(crate) format: u8,
    pub(crate) library: String,
    pub(crate) container: String,
    pub(crate) scope: Value,
    pub(crate) pass: String,
    pub(crate) root: SelectionPath,
    pub(crate) path: SelectionPath,
    pub(crate) identity: FileIdentity,
    pub(crate) generation: i64,
    pub(crate) applied_name: String,
    /// Legacy path evidence does not establish directory exclusivity.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) unclaimed: bool,
    /// Rendering recipe for a nonexclusive, multi-directory album projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) projection: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) additional_projection: Option<AlbumAdditionalProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AlbumAdditionalProjection {
    pub(crate) root: SelectionPath,
    pub(crate) templates: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AlbumDirectoryOperation {
    pub(crate) format: u8,
    pub(crate) operation: String,
    pub(crate) old: AlbumDirectoryBinding,
    pub(crate) next: AlbumDirectoryBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fallback: Option<AlbumDirectoryFallback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) staged_directory: Option<SelectionPath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) directories: Vec<AlbumDirectoryRoute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) user_entries: Vec<AlbumUserEntry>,
    /// Claims recorded by the build that installed a directory through a
    /// journaled destination claim. Decoded but never written: such a journal
    /// replays as a destination kei cannot prove, so the move refuses instead of
    /// installing it.
    #[serde(rename = "claims", default, skip_serializing)]
    pub(crate) legacy_claims: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AlbumDirectoryRoute {
    pub(crate) template: String,
    pub(crate) old: SelectionPath,
    pub(crate) next: SelectionPath,
    pub(crate) staged: SelectionPath,
    pub(crate) identity: FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AlbumUserEntry {
    pub(crate) path: SelectionPath,
    pub(crate) identity: FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AlbumDirectoryFallback {
    IndependentFiles,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AlbumDirectoryFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) native: Option<super::super::provider_generations::DirectoryPublicationSource>,
    pub(crate) library: String,
    pub(crate) child: String,
    pub(crate) version: String,
    pub(crate) provider_checksum: String,
    pub(crate) old: SelectionPath,
    pub(crate) next: SelectionPath,
    pub(crate) fingerprint: Option<LayoutFingerprint>,
    pub(crate) sidecar: Option<LayoutFingerprint>,
    /// Frozen trusted receipt, distinct from possibly user-modified source bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prior_local_checksum: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AlbumDirectorySource {
    pub(crate) native: Option<super::super::provider_generations::DirectoryPublicationSource>,
    pub(crate) library: String,
    pub(crate) child: String,
    pub(crate) version: String,
    pub(crate) provider_checksum: String,
    pub(crate) path: SelectionPath,
    pub(crate) local_checksum: String,
    pub(crate) expected_sidecar: Option<Option<LayoutFingerprint>>,
    pub(crate) created: Option<chrono::DateTime<chrono::FixedOffset>>,
}

fn sources(
    conn: &Connection,
    binding: &AlbumDirectoryBinding,
    selected_root: Option<&SelectionPath>,
) -> Result<Vec<AlbumDirectorySource>, StateError> {
    if selected_root.is_none()
        && let Some(extra) = &binding.additional_projection
    {
        let mut anchor = binding.clone();
        anchor.additional_projection = None;
        let mut result = std::collections::BTreeMap::new();
        for source in sources(conn, &anchor, None)? {
            result.insert(path_key(&source.path.to_path())?, source);
        }
        for source in sources(conn, &anchor, Some(&extra.root))? {
            if !source.path.to_path().starts_with(binding.path.to_path()) {
                result.insert(path_key(&source.path.to_path())?, source);
            }
        }
        return Ok(result.into_values().collect());
    }
    let prefix = selected_root.unwrap_or(&binding.path).to_path();
    let exact = selected_root.is_some() || binding.projection.is_some();
    let mut result = std::collections::BTreeMap::new();
    for source in super::super::provider_generations::album_directory_native_sources(
        conn,
        exact.then_some(binding.pass.as_str()),
        &prefix,
    )? {
        if (binding.unclaimed || exact)
            && !container_member(conn, binding, &source.library, &source.child)?
        {
            continue;
        }
        result.insert(path_key(&source.path.to_path())?, source);
    }
    let mut statement = conn.prepare("SELECT binding FROM primary_layout_bindings")?;
    for row in statement.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
        let layout: LayoutBinding = decode(&row?)?;
        if (binding.unclaimed || exact)
            && !container_member(conn, binding, &layout.library, &layout.child)?
        {
            continue;
        }
        if !binding.unclaimed
            && !exact
            && layout
                .files
                .iter()
                .any(|file| file.path.to_path().starts_with(&prefix))
            && layout
                .files
                .iter()
                .any(|file| !file.path.to_path().starts_with(&prefix))
        {
            return Err(invalid(
                "managed family crosses the album directory boundary",
            ));
        }
        for file in layout.files {
            if !file.path.to_path().starts_with(&prefix) {
                continue;
            }
            let key = path_key(&file.path.to_path())?;
            let current: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM primary_layout_claims WHERE path_key=?1 AND family=?2 AND operation IS NULL AND local_checksum=?3)", params![key,layout.family,data_encoding::HEXLOWER.encode(&file.fingerprint.sha256)], |row| row.get(0))?;
            if !current || file.local_preserved {
                continue;
            }
            let source = AlbumDirectorySource {
                native: None,
                library: layout.library.clone(),
                child: layout.child.clone(),
                version: file.version,
                provider_checksum: file.provider_checksum,
                path: file.path,
                local_checksum: data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),
                expected_sidecar: Some(file.sidecar),
                created: None,
            };
            if exact && !selected_source(conn, binding, &source)? {
                continue;
            }
            if let Some(old) = result.get(&key)
                && (old.library != source.library
                    || old.child != source.child
                    || old.version != source.version
                    || old.provider_checksum != source.provider_checksum
                    || old.local_checksum != source.local_checksum)
            {
                return Err(invalid("album directory has conflicting file owners"));
            }
            result.insert(key, source);
        }
    }
    let mut selected = Vec::with_capacity(result.len());
    for mut source in result.into_values() {
        if exact {
            if !selected_source(conn, binding, &source)? {
                continue;
            }
            source.created = conn
                .query_row(
                    "SELECT created_at,timezone_offset FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
                    params![source.library, source.child, source.version],
                    |row| {
                        let date = decode_asset_date(row.get(0)?, 0)?;
                        Ok(crate::state::AssetMetadata {
                            timezone_offset: row.get(1)?,
                            ..Default::default()
                        }
                        .capture_local(date))
                    },
                )
                .optional()?;
        }
        selected.push(source);
    }
    Ok(selected)
}

fn selected_source(
    conn: &Connection,
    binding: &AlbumDirectoryBinding,
    source: &AlbumDirectorySource,
) -> Result<bool, StateError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE pass_key=?1 AND asset_id=?2 AND path=?3 AND admitted=1 AND library=?4 AND version_size=?5 AND checksum=?6)", params![binding.pass,source.child,source.path.key()?,source.library,source.version,source.provider_checksum], |row| row.get(0))?)
}

fn container_member(
    conn: &Connection,
    binding: &AlbumDirectoryBinding,
    library: &str,
    child: &str,
) -> Result<bool, StateError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM asset_album_memberships m WHERE m.library=?1 AND m.container_id=?2 AND (m.asset_record_name=?3 OR EXISTS(SELECT 1 FROM legacy_master_state_owners l WHERE l.library=m.library AND l.asset_record_name=m.asset_record_name AND l.master_record_name=?3)))",params![library,binding.container,child],|row|row.get(0))?)
}

fn other_selected_owner(
    conn: &Connection,
    op: &AlbumDirectoryOperation,
    file: &AlbumDirectoryFile,
) -> Result<bool, StateError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE path=?1 AND pass_key<>?2 AND admitted=1)",params![file.old.key()?,op.old.pass],|row|row.get(0))?)
}

fn require_quiet(conn: &Connection, binding: &AlbumDirectoryBinding) -> Result<(), StateError> {
    let owned: std::collections::HashSet<_> =
        if binding.unclaimed || binding.additional_projection.is_some() {
            sources(conn, binding, None)?
                .into_iter()
                .map(|source| source.path.to_path())
                .collect()
        } else {
            Default::default()
        };
    let touches = |path: &std::path::Path| {
        if binding.unclaimed {
            owned.contains(path)
        } else {
            path.starts_with(binding.path.to_path()) || owned.contains(path)
        }
    };
    let mut pending = conn.prepare("SELECT operation,header FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled') AND policy<>?1")?;
    for row in pending.query_map([POLICY], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (operation, header) = row?;
        let header: super::LayoutOperation = decode(&header)?;
        if header.pass == binding.pass {
            return Err(invalid("finish pending album layout before renaming"));
        }
        let mut claims =
            conn.prepare("SELECT native_path FROM primary_layout_claims WHERE operation=?1")?;
        for claim in claims.query_map([operation], |row| row.get::<_, Vec<u8>>(0))? {
            let path: SelectionPath = decode(&claim?)?;
            if touches(&path.to_path()) {
                return Err(invalid("finish overlapping layout before renaming"));
            }
        }
    }
    let prefix = binding.path.to_path();
    let mut owners =
        conn.prepare("SELECT pass_key,path FROM provider_active_destinations WHERE admitted=1")?;
    for row in owners.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (pass, path) = row?;
        let path: SelectionPath = serde_json::from_str(&path)
            .map_err(|_error| invalid("invalid album ownership path"))?;
        if !binding.unclaimed && path.to_path().starts_with(&prefix) && pass != binding.pass {
            return Err(invalid("another selected pass shares the album directory"));
        }
    }
    drop(owners);
    // Dormant metadata retry markers are copied unchanged to the successor
    // receipt below. Actual selected writer obligations must be complete.
    let mut statement = conn.prepare(
        "SELECT a.local_path FROM assets a WHERE a.local_path IS NOT NULL AND a.status<>'downloaded' AND NOT EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.library=a.library AND w.asset_id=a.id AND w.version_size=a.version_size AND w.checksum=a.checksum AND w.compat_path=a.local_path AND w.admitted=1 AND w.preserved_local IS NOT NULL)",
    )?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        if touches(std::path::Path::new(&row?)) {
            return Err(invalid(
                "finish pending album file work before renaming its directory",
            ));
        }
    }
    let mut statement = conn.prepare("SELECT w.path FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.admitted=1 AND ((w.verified_media=0 AND w.preserved_local IS NULL) OR (g.metadata_enabled=1 AND w.verified_metadata=0))")?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        let path: SelectionPath =
            serde_json::from_str(&row?).map_err(|_error| invalid("invalid selected album path"))?;
        if touches(&path.to_path()) {
            return Err(invalid(
                "finish selected album work before renaming its directory",
            ));
        }
    }
    Ok(())
}

pub(crate) fn same_container(left: &Value, right: &Value) -> bool {
    let (Some(mut left), Some(mut right)) = (left.as_object().cloned(), right.as_object().cloned())
    else {
        return false;
    };
    left.remove("name");
    right.remove("name");
    left.get("container_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
        && left == right
}

fn validate_binding(binding: &AlbumDirectoryBinding) -> Result<(), StateError> {
    let root = binding.root.to_path();
    let path = binding.path.to_path();
    if let Some(extra) = &binding.additional_projection {
        let extra_root = extra.root.to_path();
        if binding.projection.is_some()
            || !extra_root.is_absolute()
            || !root.starts_with(&extra_root)
            || extra.templates.is_empty()
            || crate::fs_util::absolute_lexical(&extra_root)
                .map_or(true, |normalized| normalized != extra_root)
            || extra.templates.iter().any(|template| {
                chrono::format::StrftimeItems::new(template)
                    .any(|item| item == chrono::format::Item::Error)
            })
        {
            return Err(invalid("invalid additional album projection"));
        }
    }
    if let Some(templates) = &binding.projection
        && (templates.is_empty()
            || templates.iter().any(|template| {
                chrono::format::StrftimeItems::new(template)
                    .any(|item| item == chrono::format::Item::Error)
            }))
    {
        return Err(invalid("invalid album projection recipe"));
    }
    if binding.format != 1
        || binding.generation < 1
        || !root.is_absolute()
        || (path == root && binding.projection.is_none())
        || (binding.projection.is_some() && (!binding.unclaimed || path != root))
        || !path.starts_with(&root)
        || crate::fs_util::absolute_lexical(&path)
            .map_err(|_error| invalid("invalid album directory path"))?
            != path
        || binding.applied_name.trim().is_empty()
        || binding.scope.get("container_id").and_then(Value::as_str)
            != Some(binding.container.as_str())
        || binding
            .scope
            .get("zone")
            .and_then(|zone| zone.get("zoneName"))
            .and_then(Value::as_str)
            != Some(binding.library.as_str())
        || binding.scope.get("name").and_then(Value::as_str) != Some(binding.applied_name.as_str())
    {
        return Err(invalid("invalid exclusive album directory binding"));
    }
    Ok(())
}

fn family(binding: &AlbumDirectoryBinding) -> Result<String, StateError> {
    Ok(format!(
        "album-directory:{:x}",
        Sha256::digest(encode(&(
            &binding.library,
            &binding.container,
            &binding.root,
        ))?)
    ))
}

pub(crate) fn relocated_path(
    op: &AlbumDirectoryOperation,
    old: &SelectionPath,
) -> Result<SelectionPath, StateError> {
    let old = old.to_path();
    if op.old.projection.is_some() || !old.starts_with(op.old.path.to_path()) {
        let route = op
            .directories
            .iter()
            .find(|route| old.parent() == Some(route.old.to_path().as_path()))
            .ok_or_else(|| invalid("album member has no exact directory route"))?;
        let name = old
            .file_name()
            .ok_or_else(|| invalid("album member has no filename"))?;
        return Ok(SelectionPath::from_path(&route.next.to_path().join(name)));
    }
    let prefix = op.old.path.to_path();
    let relative = old
        .strip_prefix(&prefix)
        .map_err(|_error| invalid("album member escapes its directory"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(invalid("album member is the directory itself"));
    }
    Ok(SelectionPath::from_path(
        &op.next.path.to_path().join(relative),
    ))
}

pub(in crate::state::db) fn load_operation(
    conn: &Connection,
    operation: &str,
) -> Result<(AlbumDirectoryOperation, String), StateError> {
    let (header, phase): (Vec<u8>, String) = conn.query_row(
        "SELECT header,phase FROM primary_layout_operations WHERE operation=?1 AND policy=?2",
        params![operation, POLICY],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let op: AlbumDirectoryOperation = decode(&header)?;
    let normalized: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM primary_layout_operations WHERE operation=?1 AND family=?2 AND library=?3 AND source_library=?3 AND prior_generation=?4 AND decision=?5 AND policy=?6)",
        params![operation,family(&op.old)?,op.old.library,op.old.generation,op.next.pass,POLICY], |row| row.get(0),
    )?;
    validate_binding(&op.old)?;
    validate_binding(&op.next)?;
    let mut parents = std::collections::HashSet::new();
    let route_root = op
        .old
        .additional_projection
        .as_ref()
        .map_or(&op.old.root, |extra| &extra.root)
        .to_path();
    for (index, route) in op.directories.iter().enumerate() {
        let overlaps = |left: &std::path::Path, right: &std::path::Path| {
            left.starts_with(right) || right.starts_with(left)
        };
        if op.old.additional_projection.is_some()
            && [&route.old, &route.next, &route.staged].iter().any(|path| {
                overlaps(&path.to_path(), &op.old.path.to_path())
                    || overlaps(&path.to_path(), &op.next.path.to_path())
            })
        {
            return Err(invalid("external album route overlaps its anchor"));
        }
        if op
            .directories
            .iter()
            .take(index)
            .any(|previous| overlaps(&route.next.to_path(), &previous.next.to_path()))
        {
            return Err(invalid("album route destinations overlap"));
        }
        let expected = route.next.to_path().with_file_name(format!(
            ".kei-album-stage-{}-{index}",
            op.operation
                .strip_prefix("album-directory:")
                .ok_or_else(|| invalid("invalid album route identity"))?
        ));
        if chrono::format::StrftimeItems::new(&route.template)
            .any(|item| item == chrono::format::Item::Error)
            || (op.old.projection.is_none() && op.old.additional_projection.is_none())
            || op.fallback.is_none()
            || !parents.insert(route.old.key()?)
            || route.staged.to_path() != expected
            || route.old == route.next
            || [&route.old, &route.next, &route.staged].iter().any(|path| {
                let path = path.to_path();
                path == route_root
                    || !path.starts_with(&route_root)
                    || crate::fs_util::absolute_lexical(&path)
                        .map_or(true, |normalized| normalized != path)
            })
        {
            return Err(invalid("invalid album file route"));
        }
    }
    if let Some(staged) = &op.staged_directory {
        let id = op
            .operation
            .strip_prefix("album-directory:")
            .ok_or_else(|| invalid("invalid staged album operation"))?;
        let id =
            uuid::Uuid::parse_str(id).map_err(|_error| invalid("invalid staged album identity"))?;
        let expected = op
            .next
            .path
            .to_path()
            .with_file_name(format!(".kei-album-stage-{id}"));
        if op.fallback.is_none() || staged.to_path() != expected || staged == &op.next.path {
            return Err(invalid("invalid staged album directory"));
        }
    }
    if !matches!(phase.as_str(), "publishing" | "committed" | "cancelled")
        || !normalized
        || op.format != 1
        || op.operation != operation
        || op.old.root != op.next.root
        || op.old.additional_projection != op.next.additional_projection
        || (op.old.path == op.next.path && op.old.projection.is_none())
        || op.old.applied_name == op.next.applied_name
        || (op.fallback.is_none() && (op.old.unclaimed || op.old.identity != op.next.identity))
        || (op.fallback.is_none() && op.next.unclaimed)
        || (op.old.projection.is_some()
            && (op.fallback.is_none()
                || op.next.projection != op.old.projection
                || !op.user_entries.is_empty()))
        || Some(op.next.generation) != op.old.generation.checked_add(1)
        || !same_container(&op.old.scope, &op.next.scope)
    {
        return Err(invalid("invalid album directory journal"));
    }
    if !op.legacy_claims.is_empty() {
        tracing::warn!(
            operation = %operation,
            claims = op.legacy_claims.len(),
            "Album directory journal carries retired destination claims; its move will refuse"
        );
    }
    Ok((op, phase))
}

fn path_available(conn: &Connection, path: &SelectionPath) -> Result<bool, StateError> {
    let directory = path.to_path();
    if fences_path(conn, &directory)? {
        return Ok(false);
    }
    let mut historical = conn.prepare("SELECT local_path FROM assets WHERE local_path IS NOT NULL UNION SELECT local_path FROM asset_metadata_paths UNION SELECT destination_path FROM reconciliation_paths")?;
    for row in historical.query_map([], |row| row.get::<_, String>(0))? {
        if std::path::Path::new(&row?).starts_with(&directory) {
            return Ok(false);
        }
    }
    drop(historical);
    let mut selected = conn.prepare("SELECT path FROM provider_active_destinations")?;
    for row in selected.query_map([], |row| row.get::<_, String>(0))? {
        let reserved: SelectionPath = serde_json::from_str(&row?)
            .map_err(|_error| invalid("invalid historical album destination"))?;
        if reserved.to_path().starts_with(&directory) {
            return Ok(false);
        }
    }
    drop(selected);
    let mut statement = conn.prepare(
        "SELECT directory_binding FROM album_containers WHERE directory_binding IS NOT NULL",
    )?;
    for row in statement.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
        let binding: AlbumDirectoryBinding = decode(&row?)?;
        if path_key(&binding.path.to_path())? == path_key(&directory)? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl SqliteStateDb {
    pub(crate) async fn remember_album_additional_projection(
        &self,
        owner: account::AccountOwner,
        mut binding: AlbumDirectoryBinding,
        root: SelectionPath,
        template: String,
    ) -> Result<AlbumDirectoryBinding, StateError> {
        self.with_conn_mut("remember_album_additional_projection", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let before = encode(&binding)?;
            let extra = binding.additional_projection.get_or_insert_with(|| AlbumAdditionalProjection {
                root: root.clone(), templates: Vec::new(),
            });
            if extra.root != root { return Err(invalid("album projection root changed")); }
            if !extra.templates.contains(&template) { extra.templates.push(template); }
            validate_binding(&binding)?;
            let after = encode(&binding)?;
            if after != before {
                let changed = conn.execute("UPDATE album_containers SET directory_binding=?4 WHERE library=?1 AND container_id=?2 AND directory_binding=?3", params![binding.library,binding.container,before,after])?;
                if changed != 1 { return Err(invalid("album projection changed during planning")); }
            }
            Ok(binding)
        }).await
    }
    pub(crate) async fn remember_album_projection(
        &self,
        owner: account::AccountOwner,
        mut binding: AlbumDirectoryBinding,
        template: String,
    ) -> Result<AlbumDirectoryBinding, StateError> {
        self.with_conn_mut("remember_album_projection", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let before = encode(&binding)?;
            let templates = binding.projection.as_mut().ok_or_else(|| invalid("missing album projection"))?;
            if !templates.contains(&template) {
                templates.push(template);
                let changed = conn.execute("UPDATE album_containers SET directory_binding=?4 WHERE library=?1 AND container_id=?2 AND directory_binding=?3", params![binding.library,binding.container,before,encode(&binding)?])?;
                if changed != 1 { return Err(invalid("album projection changed during planning")); }
            }
            Ok(binding)
        }).await
    }

    pub(crate) async fn album_projection_routes(
        &self,
        owner: account::AccountOwner,
        binding: AlbumDirectoryBinding,
    ) -> Result<Vec<AlbumDirectoryRoute>, StateError> {
        self.with_conn("album_projection_routes", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            Ok(projection_operation(conn, &binding)?.map_or_else(Vec::new, |op| op.directories))
        })
        .await
    }
    pub(crate) async fn album_projection_files(
        &self,
        owner: account::AccountOwner,
        binding: AlbumDirectoryBinding,
        root: SelectionPath,
    ) -> Result<Vec<AlbumDirectoryFile>, StateError> {
        self.with_conn("album_projection_files", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let mut files: Vec<AlbumDirectoryFile> = Vec::new();
            if let Some(op) = projection_operation(conn, &binding)? {
                let mut members = conn.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member")?;
                files = members.query_map([op.operation], |row| row.get::<_,Vec<u8>>(0))?.map(|row| decode(&row?)).collect::<Result<_,_>>()?;
            }
            for source in sources(conn, &binding, Some(&root))? {
                if files.iter().any(|file| file.library == source.library && file.child == source.child && file.version == source.version) { continue; }
                files.push(AlbumDirectoryFile { native: source.native, library: source.library, child: source.child, version: source.version, provider_checksum: source.provider_checksum, old: source.path.clone(), next: source.path, fingerprint: None, sidecar: None, prior_local_checksum: Some(source.local_checksum) });
            }
            Ok(files)
        }).await
    }

    pub(crate) async fn album_directory_has_preserved_history(
        &self,
        owner: account::AccountOwner,
        binding: AlbumDirectoryBinding,
    ) -> Result<bool, StateError> {
        self.with_conn("album_directory_has_preserved_history", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let mut statement = conn.prepare("SELECT evidence FROM primary_layout_preserved")?;
            for row in statement.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
                let file: super::LayoutFile = decode(&row?)?;
                if file.path.to_path().starts_with(binding.path.to_path()) {
                    return Ok(true);
                }
            }
            let mut statement = conn.prepare(
                "SELECT path FROM provider_active_destinations WHERE preserved_local IS NOT NULL",
            )?;
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                let path: SelectionPath = serde_json::from_str(&row?)
                    .map_err(|_error| invalid("invalid preserved album path"))?;
                if path.to_path().starts_with(binding.path.to_path()) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
    }

    pub(crate) async fn album_directory_has_unsettled_publication(
        &self,
        owner: account::AccountOwner,
        binding: AlbumDirectoryBinding,
    ) -> Result<bool, StateError> {
        self.with_conn(
            "checking unfinished native directory handoff",
            move |conn| {
                account::validate_authenticated(conn, &owner)?;
                super::super::provider_generations::has_unsettled_directory_publication(
                    conn, &binding,
                )
            },
        )
        .await
    }
    /// Legacy per-file ownership does not establish exclusive directory ownership.
    /// Keep its recorded paths under an unclaimed binding until a rename.
    pub(crate) async fn album_directory_has_legacy_paths(
        &self,
        owner: account::AccountOwner,
        library: String,
        container: String,
        directory: SelectionPath,
    ) -> Result<bool, StateError> {
        self.with_conn("album_directory_has_legacy_paths", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let mut statement = conn.prepare("SELECT p.local_path FROM asset_metadata_paths p JOIN asset_album_memberships m ON m.library=p.library AND (m.asset_record_name=p.id OR EXISTS(SELECT 1 FROM legacy_master_state_owners l WHERE l.library=p.library AND l.master_record_name=p.id AND l.asset_record_name=m.asset_record_name)) WHERE m.library=?1 AND m.container_id=?2")?;
            for row in statement.query_map(params![library,container], |row| row.get::<_,String>(0))? {
                if std::path::Path::new(&row?).starts_with(directory.to_path()) { return Ok(true); }
            }
            Ok(false)
        }).await
    }

    pub(crate) async fn has_album_directory_debt(
        &self,
        owner: account::AccountOwner,
        library: String,
        containers: Vec<String>,
    ) -> Result<bool, StateError> {
        self.with_conn("has_album_directory_debt", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let mut statement = conn.prepare("SELECT operation FROM primary_layout_operations WHERE policy=?1 AND library=?2 AND phase NOT IN ('committed','cancelled')")?;
            for row in statement.query_map(params![POLICY,library], |row| row.get::<_,String>(0))? {
                let (op, _) = load_operation(conn, &row?)?;
                if containers.contains(&op.old.container) { return Ok(true); }
            }
            Ok(false)
        }).await
    }
    pub(crate) async fn album_directory_path_available(
        &self,
        owner: account::AccountOwner,
        path: SelectionPath,
    ) -> Result<bool, StateError> {
        self.with_conn("album_directory_path_available", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            path_available(conn, &path)
        })
        .await
    }

    pub(crate) async fn cancel_unmoved_album_directory(
        &self,
        owner: account::AccountOwner,
        operation: String,
        source_identity: FileIdentity,
    ) -> Result<(), StateError> {
        self.with_conn_mut("cancel_unmoved_album_directory", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let (op, phase) = load_operation(&tx, &operation)?;
            if phase != "publishing" || source_identity != op.old.identity { return Err(invalid("album move cannot be cancelled after publication")); }
            tx.execute("UPDATE primary_layout_operations SET phase='cancelled',conflict='unmoved_replan' WHERE operation=?1",[operation])?;
            tx.commit()?;
            Ok(())
        }).await
    }
    /// Commit a verified directory move or independent-file routing intent.
    /// Fallback routing transfers no file receipts; normal publication must
    /// complete before successor admission can withdraw the old selection.
    pub(crate) async fn publish_album_directory(
        &self,
        owner: account::AccountOwner,
        operation: String,
        verified: Vec<AlbumDirectoryFile>,
    ) -> Result<(), StateError> {
        self.with_conn_mut("publish_album_directory", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let (op, phase) = load_operation(&tx, &operation)?;
            if phase == "committed" { return Ok(()); }
            if phase != "publishing" { return Err(invalid("album directory is not publishing")); }
            let stored: Vec<u8> = tx.query_row("SELECT directory_binding FROM album_containers WHERE library=?1 AND container_id=?2", params![op.old.library,op.old.container], |row| row.get(0))?;
            if stored != encode(&op.old)? { return Err(invalid("album binding changed during rename")); }
            let members: Vec<Vec<u8>> = tx.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member")?.query_map([&operation], |row| row.get(0))?.collect::<Result<_,_>>()?;
            if members.len() != verified.len() || verified.iter().zip(&members).any(|(file, bytes)| encode(file).map_or(true, |actual| actual != *bytes)) {
                return Err(invalid("relocated album verification differs from journal"));
            }
            for file in &verified {
                if op.fallback.is_none() {
                    super::super::provider_generations::land_directory_publication(&tx, file)?;
                }
                let old = file.old.to_path();
                let next = file.next.to_path();
                // Append a current path receipt with the same proven provenance;
                // never update or delete the historical path row.
                if op.fallback.is_none() && let Some(fingerprint) = &file.fingerprint
                    && let (Some(old), Some(next)) = (old.to_str(), next.to_str()) {
                    let checksum = data_encoding::HEXLOWER.encode(&fingerprint.sha256);
                    if file.prior_local_checksum.as_ref().is_none_or(|prior| prior == &checksum) {
                        tx.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size,source_checksum,embedded_metadata_receipt) SELECT library,id,version_size,?5,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size,source_checksum,embedded_metadata_receipt FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4 AND provider_checksum=?6 AND local_checksum=?7", params![file.library,file.child,file.version,old,next,file.provider_checksum,checksum])?;
                        tx.execute("UPDATE assets SET local_path=?5 WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4 AND local_checksum=?6 AND checksum=?7", params![file.library,file.child,file.version,old,next,checksum,file.provider_checksum])?;
                    }
                }
                if (op.old.unclaimed || !old.starts_with(op.old.path.to_path())) && other_selected_owner(&tx, &op, file)? { continue; }
                tx.execute("INSERT OR IGNORE INTO primary_layout_superseded_paths(library,child,version,provider_checksum,native_path,compat_path) VALUES(?1,?2,?3,?4,?5,?6)",params![file.library,file.child,file.version,file.provider_checksum,encode(&file.old)?,old.to_str()])?;
                // Keep the obsolete slot fenced even if a user later recreates
                // its directory. The new slot is owned by its successor family.
                tx.execute("UPDATE primary_layout_claims SET operation=?2 WHERE path_key=?1",params![path_key(&file.old.to_path())?,operation])?;
            }
            let bindings: Vec<Vec<u8>> = if op.fallback.is_none() {
                tx.prepare("SELECT binding FROM primary_layout_bindings")?.query_map([], |row| row.get(0))?.collect::<Result<_,_>>()?
            } else { Vec::new() };
            for bytes in bindings {
                let old: LayoutBinding = decode(&bytes)?;
                if !old.files.iter().any(|file| file.path.to_path().starts_with(op.old.path.to_path())) { continue; }
                if old.files.iter().any(|file| !file.path.to_path().starts_with(op.old.path.to_path())) {
                    return Err(invalid("managed family crosses the album directory boundary"));
                }
                let mut next = old.clone();
                for file in &mut next.files { file.path = relocated_path(&op, &file.path)?; }
                let first = next.files.first().ok_or_else(|| invalid("empty managed album family"))?.path.to_path();
                let parent = SelectionPath::from_path(first.parent().ok_or_else(|| invalid("managed album family has no parent"))?);
                next.family = format!("{:x}", Sha256::digest(serde_json::to_vec(&serde_json::json!([1,old.library,old.child,op.next.pass,parent])).map_err(|_error| invalid("invalid successor family"))?));
                tx.execute("INSERT INTO primary_layout_bindings(family,library,source_library,child,generation,decision,policy,binding) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![next.family,next.library,next.source_library,next.child,next.generation,next.decision,next.policy,encode(&next)?])?;
                for file in &next.files {
                    tx.execute("INSERT INTO primary_layout_claims(path_key,family,operation,library,child,version,provider_checksum,local_checksum,native_path) VALUES(?1,?2,NULL,?3,?4,?5,?6,?7,?8)",params![path_key(&file.path.to_path())?,next.family,next.library,next.child,file.version,file.provider_checksum,data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),encode(&file.path)?])?;
                }
            }
            tx.execute("UPDATE album_containers SET directory_binding=?3 WHERE library=?1 AND container_id=?2",params![op.next.library,op.next.container,encode(&op.next)?])?;
            tx.execute("UPDATE primary_layout_operations SET phase='committed' WHERE operation=?1",[operation])?;
            tx.commit()?;
            Ok(())
        }).await
    }
    pub(crate) async fn album_directory_sources(
        &self,
        owner: account::AccountOwner,
        binding: AlbumDirectoryBinding,
    ) -> Result<Vec<AlbumDirectorySource>, StateError> {
        self.with_conn("album_directory_sources", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            require_quiet(conn, &binding)?;
            sources(conn, &binding, None)
        })
        .await
    }

    pub(crate) async fn begin_album_directory_rename(
        &self,
        owner: account::AccountOwner,
        op: AlbumDirectoryOperation,
        files: Vec<AlbumDirectoryFile>,
    ) -> Result<(), StateError> {
        self.with_conn_mut("begin_album_directory_rename", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            validate_binding(&op.old)?;
            validate_binding(&op.next)?;
            if op.old.projection.is_none() && !path_available(&tx, &op.next.path)? { return Err(invalid("album target has durable historical ownership")); }
            for route in &op.directories { if !path_available(&tx, &route.next)? { return Err(invalid("album file target has durable historical ownership")); } }
            require_quiet(&tx, &op.old)?;
            let stored: Vec<u8> = tx.query_row("SELECT directory_binding FROM album_containers WHERE library=?1 AND container_id=?2", params![op.old.library,op.old.container], |row| row.get(0))?;
            if stored != encode(&op.old)? { return Err(invalid("album binding changed before rename")); }
            let expected = sources(&tx, &op.old, None)?;
            for entry in &op.user_entries {
                relocated_path(&op, &entry.path)?;
                if op.fallback.is_none() || op.old.unclaimed || expected.iter().any(|source| {
                    let source = source.path.to_path();
                    let mut sidecar = source.as_os_str().to_owned(); sidecar.push(".xmp");
                    source.starts_with(entry.path.to_path()) || std::path::Path::new(&sidecar).starts_with(entry.path.to_path())
                }) {
                    return Err(invalid("user entry overlaps managed album media"));
                }
            }
            if expected.len() != files.len() { return Err(invalid("album file census changed")); }
            for (source, file) in expected.iter().zip(&files) {
                if source.path != file.old || file.next != relocated_path(&op, &file.old)?
                    || source.library != file.library || source.child != file.child
                    || source.version != file.version || source.provider_checksum != file.provider_checksum
                    || source.native != file.native
                    || if op.fallback.is_some() {
                        file.prior_local_checksum.as_ref() != Some(&source.local_checksum)
                    } else {
                        file.prior_local_checksum.is_some() || file.fingerprint.as_ref().is_none_or(|fingerprint| source.local_checksum != data_encoding::HEXLOWER.encode(&fingerprint.sha256))
                    } {
                    return Err(invalid("album file receipt does not match rename input"));
                }
            }
            tx.execute("INSERT INTO primary_layout_operations(operation,family,library,source_library,prior_generation,decision,policy,phase,header) VALUES(?1,?2,?3,?3,?4,?5,?6,'publishing',?7)", params![op.operation,family(&op.old)?,op.old.library,op.old.generation,op.next.pass,POLICY,encode(&op)?])?;
            load_operation(&tx, &op.operation)?;
            for (index, file) in files.iter().enumerate() {
                tx.execute("INSERT INTO primary_layout_members(operation,member,evidence) VALUES(?1,?2,?3)", params![op.operation,i64::try_from(index).map_err(|_error| invalid("too many album files"))?,encode(file)?])?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn album_directory_files(
        &self,
        owner: account::AccountOwner,
        operation: String,
    ) -> Result<Vec<AlbumDirectoryFile>, StateError> {
        self.with_conn("album_directory_files", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let (op, _) = load_operation(conn, &operation)?;
            let mut statement = conn.prepare(
                "SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member",
            )?;
            statement
                .query_map([operation], |row| row.get::<_, Vec<u8>>(0))?
                .map(|row| {
                    let file: AlbumDirectoryFile = decode(&row?)?;
                    if relocated_path(&op, &file.old)? != file.next {
                        return Err(invalid("album member path changed"));
                    }
                    Ok(file)
                })
                .collect()
        })
        .await
    }
    pub(crate) async fn album_directory_binding(
        &self,
        owner: account::AccountOwner,
        library: String,
        container: String,
    ) -> Result<Option<AlbumDirectoryBinding>, StateError> {
        self.with_conn("album_directory_binding", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let bytes: Option<Vec<u8>> = conn.query_row(
                "SELECT directory_binding FROM album_containers WHERE library=?1 AND container_id=?2",
                params![library, container], |row| row.get(0),
            ).optional()?.flatten();
            bytes.map(|bytes| {
                let binding: AlbumDirectoryBinding = decode(&bytes)?;
                validate_binding(&binding)?;
                if binding.library != library || binding.container != container {
                    return Err(invalid("album directory owner mismatch"));
                }
                Ok(binding)
            }).transpose()
        }).await
    }

    /// Called only after exclusive directory creation, identity validation and
    /// parent durability. An existing unbound directory is never adopted here.
    pub(crate) async fn bind_created_album_directory(
        &self,
        owner: account::AccountOwner,
        binding: AlbumDirectoryBinding,
    ) -> Result<(), StateError> {
        self.with_conn_mut("bind_created_album_directory", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            validate_binding(&binding)?;
            if binding.generation != 1 {
                return Err(invalid("initial album directory generation is not one"));
            }
            if !binding.unclaimed && !path_available(&tx, &binding.path)? { return Err(invalid("album directory has durable historical ownership")); }
            let updated = tx.execute(
                "INSERT INTO album_containers(library,container_id,album_name,pass_kind,is_deleted,updated_at,directory_binding) VALUES(?1,?2,?3,'album',0,?4,?5) ON CONFLICT(library,container_id) DO UPDATE SET directory_binding=excluded.directory_binding WHERE album_containers.directory_binding IS NULL",
                params![binding.library,binding.container,binding.applied_name,chrono::Utc::now().timestamp(),encode(&binding)?],
            )?;
            if updated != 1 { return Err(invalid("album directory binding changed")); }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub(crate) async fn album_directory_operation(
        &self,
        owner: account::AccountOwner,
        binding: AlbumDirectoryBinding,
    ) -> Result<Option<(AlbumDirectoryOperation, String)>, StateError> {
        self.with_conn("album_directory_operation", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let operation: Option<String> = conn.query_row(
                "SELECT operation FROM primary_layout_operations WHERE family=?1 AND policy=?2 AND phase NOT IN ('committed','cancelled')",
                params![family(&binding)?, POLICY], |row| row.get(0),
            ).optional()?;
            operation.map(|operation| load_operation(conn, &operation)).transpose()
        }).await
    }
}

fn projection_operation(
    conn: &Connection,
    binding: &AlbumDirectoryBinding,
) -> Result<Option<AlbumDirectoryOperation>, StateError> {
    let mut operations = conn.prepare("SELECT operation FROM primary_layout_operations WHERE family=?1 AND policy=?2 AND phase='committed'")?;
    for operation in operations.query_map(params![family(binding)?, POLICY], |row| {
        row.get::<_, String>(0)
    })? {
        let (op, _) = load_operation(conn, &operation?)?;
        if op.next.generation == binding.generation && op.next.scope == binding.scope {
            return Ok(Some(op));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{AlbumDirectoryBinding, AlbumDirectoryFallback, AlbumDirectoryOperation};
    use crate::fs_util::{ConfinedParents, ConfinedPath, FileIdentity};
    use crate::state::db::primary_layout::{decode, encode};
    use crate::state::db::provider_selection::SelectionPath;

    fn identity(root: &std::path::Path, path: &std::path::Path) -> FileIdentity {
        ConfinedPath::open(root, path, ConfinedParents::Existing)
            .unwrap()
            .entry_identity()
            .unwrap()
    }

    fn binding(
        root: &std::path::Path,
        path: &std::path::Path,
        identity: FileIdentity,
        generation: i64,
        applied_name: &str,
    ) -> AlbumDirectoryBinding {
        AlbumDirectoryBinding {
            format: 1,
            library: "PrimarySync".into(),
            container: "container-1".into(),
            scope: serde_json::json!({
                "container_id": "container-1",
                "zone": { "zoneName": "PrimarySync" },
                "name": applied_name,
            }),
            pass: "album-pass".into(),
            root: SelectionPath::from_path(root),
            path: SelectionPath::from_path(path),
            identity,
            generation,
            applied_name: applied_name.into(),
            unclaimed: false,
            projection: None,
            additional_projection: None,
        }
    }

    /// A journal header round trips the destinations and its format marker.
    #[test]
    fn album_directory_header_round_trips_its_destinations() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("media/Old");
        let next = root.path().join("media/New");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&next).unwrap();
        let old_identity = identity(root.path(), &old);
        let next_identity = identity(root.path(), &next);
        let op = AlbumDirectoryOperation {
            format: 1,
            operation: "album-directory:0b7f4d0e-6b0e-4a49-8d3f-2f6f4c1c0f11".into(),
            old: binding(root.path(), &old, old_identity, 1, "Old"),
            next: binding(root.path(), &next, next_identity, 2, "New"),
            fallback: Some(AlbumDirectoryFallback::IndependentFiles),
            staged_directory: None,
            directories: Vec::new(),
            user_entries: Vec::new(),
            legacy_claims: Vec::new(),
        };

        let encoded = encode(&op).unwrap();
        let decoded: AlbumDirectoryOperation = decode(&encoded).unwrap();
        assert_eq!(decoded.format, 1);
        assert_eq!(decoded.old.path, SelectionPath::from_path(&old));
        assert_eq!(decoded.next.path, SelectionPath::from_path(&next));
        assert_eq!(decoded.old.identity, old_identity);
        assert_eq!(decoded.next.identity, next_identity);
        assert!(
            !String::from_utf8_lossy(&encoded).contains("claims"),
            "a retired claim field must never be written again"
        );

        // A header written by the build that recorded claims must still load,
        // so replay refuses that destination instead of failing the database.
        let mut legacy: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        legacy.as_object_mut().unwrap().insert(
            "claims".into(),
            serde_json::json!([{ "path": "media/New", "identity": {"device": 1, "inode": 2} }]),
        );
        let legacy = serde_json::to_vec(&legacy).unwrap();
        let decoded: AlbumDirectoryOperation = decode(&legacy).unwrap();
        assert_eq!(decoded.format, 1);
        assert_eq!(decoded.legacy_claims.len(), 1);
        assert_eq!(decoded.old.path, SelectionPath::from_path(&old));
    }
}
