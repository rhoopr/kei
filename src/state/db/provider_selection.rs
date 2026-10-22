//! Account-bound shadow selection manifests. No queue or checkpoint authority.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::provider_inbox::{CapturedPageId, load_page};
use super::{SqliteStateDb, account};
use crate::state::{VersionSizeKey, error::StateError};

pub(crate) const MAX_SELECTION_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MANIFEST_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionSource {
    pub(crate) page_id: i64,
    pub(crate) ordinal: i64,
    pub(crate) body_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionDestination {
    pub(crate) version_size: String,
    pub(crate) path: SelectionPath,
    pub(crate) checksum: String,
    pub(crate) size: u64,
    pub(crate) metadata_hash: String,
}

/// Preserve native path identity without imposing a new UTF-8 root policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "path", rename_all = "snake_case")]
pub(crate) enum SelectionPath {
    Utf8(String),
    #[cfg(unix)]
    UnixBytes(Vec<u8>),
    #[cfg(windows)]
    WindowsUnits(Vec<u16>),
}

impl SelectionPath {
    pub(crate) fn from_path(path: &std::path::Path) -> Self {
        if let Some(path) = path.to_str() {
            return Self::Utf8(path.to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::UnixBytes(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::WindowsUnits(path.as_os_str().encode_wide().collect())
        }
    }

    pub(crate) fn to_path(&self) -> std::path::PathBuf {
        match self {
            Self::Utf8(path) => std::path::PathBuf::from(path),
            #[cfg(unix)]
            Self::UnixBytes(bytes) => {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(bytes.clone()).into()
            }
            #[cfg(windows)]
            Self::WindowsUnits(units) => {
                use std::os::windows::ffi::OsStringExt;
                std::ffi::OsString::from_wide(units).into()
            }
        }
    }

    pub(crate) fn key(&self) -> Result<String, StateError> {
        serde_json::to_string(self).map_err(|_invalid| StateError::ProviderSelectionInvalid)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SelectionOutcome {
    Selected,
    Excluded,
    Deferred,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionDecision {
    pub(crate) pass_key: String,
    pub(crate) child: String,
    pub(crate) master: Option<String>,
    /// Original response bytes, including unknowns and numeric lexemes.
    pub(crate) confirmation: Option<Vec<u8>>,
    pub(crate) outcome: SelectionOutcome,
    /// Internal diagnostic, never provider error text.
    pub(crate) reason: String,
    pub(crate) destinations: Vec<SelectionDestination>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SelectionManifest {
    pub(crate) scope: String,
    pub(crate) zone: serde_json::Value,
    pub(crate) config_hash: String,
    /// Versioned policy evidence supplied by the selection owner. This stage
    /// covers individual confirmed sources, never inventory or complete scope.
    pub(crate) profile: serde_json::Value,
    pub(crate) sources: Vec<SelectionSource>,
    pub(crate) decisions: Vec<SelectionDecision>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn invalid<T>() -> Result<T, StateError> {
    Err(StateError::ProviderSelectionInvalid)
}

/// Strict provider-owned equivalence between a retained selection manifest and
/// the current one. Every non-confirmation field stays exact. A paired lookup
/// confirmation may differ only in validated private `ASSETID` URL tokens
/// classified by the provider-owned transport evidence rule.
fn manifest_equivalent(stored: &SelectionManifest, current: &SelectionManifest) -> bool {
    if stored == current {
        return true;
    }
    if stored.scope != current.scope
        || stored.zone != current.zone
        || stored.config_hash != current.config_hash
        || stored.profile != current.profile
        || stored.sources != current.sources
        || stored.decisions.len() != current.decisions.len()
    {
        return false;
    }
    stored
        .decisions
        .iter()
        .zip(&current.decisions)
        .all(|(stored, current)| {
            if stored.pass_key != current.pass_key
                || stored.child != current.child
                || stored.master != current.master
                || stored.outcome != current.outcome
                || stored.reason != current.reason
                || stored.destinations != current.destinations
            {
                return false;
            }
            match (&stored.confirmation, &current.confirmation) {
                (None, None) => true,
                (Some(stored_body), Some(current_body)) => {
                    let Some(master) = current.master.as_deref() else {
                        return false;
                    };
                    let Ok(Some(stored_evidence)) =
                        crate::icloud::photos::transport_evidence::classify_lookup_confirmation(
                            stored_body,
                            &current.child,
                            master,
                        )
                    else {
                        return false;
                    };
                    let Ok(Some(current_evidence)) =
                        crate::icloud::photos::transport_evidence::classify_lookup_confirmation(
                            current_body,
                            &current.child,
                            master,
                        )
                    else {
                        return false;
                    };
                    stored_evidence.version == current_evidence.version
                        && stored_evidence.masked == current_evidence.masked
                }
                _ => false,
            }
        })
}

/// One bounded manifest body at a time. Candidates are narrowed by exact
/// account/provider/config/scope/source/decision metadata, then compared
/// through the strict provider-owned equivalence rule before any new charge.
/// The full matching set is scanned so a legitimate class is never treated as
/// absent merely because it sorts after an arbitrary prefix.
fn equivalent_generation(
    tx: &rusqlite::Transaction<'_>,
    manifest: &SelectionManifest,
    account_key: &str,
    provider_key: &str,
) -> Result<Option<String>, StateError> {
    if manifest.sources.len() != 1 || manifest.decisions.len() != 1 {
        return Ok(None);
    }
    let source = manifest
        .sources
        .first()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let decision = manifest
        .decisions
        .first()
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if decision.confirmation.is_none() {
        return Ok(None);
    }
    let mut statement = tx.prepare(
        "SELECT g.id,g.manifest FROM provider_selection_generations g
         WHERE g.account_key=?1 AND g.provider_key=?2 AND g.format=?3 AND g.scope=?4 AND g.config_hash=?5
           AND (SELECT count(*) FROM provider_selection_sources s WHERE s.generation=g.id)=1
           AND (SELECT count(*) FROM provider_selection_decisions d WHERE d.generation=g.id)=1
           AND EXISTS(SELECT 1 FROM provider_selection_sources s WHERE s.generation=g.id AND s.page_id=?6 AND s.ordinal=?7 AND s.body_hash=?8)
           AND EXISTS(SELECT 1 FROM provider_selection_decisions d WHERE d.generation=g.id AND d.pass_key=?9 AND d.child=?10 AND d.outcome=?11 AND d.reason=?12)
         ORDER BY g.id",
    )?;
    let mut rows = statement.query(params![
        account_key,
        provider_key,
        MANIFEST_VERSION,
        manifest.scope,
        manifest.config_hash,
        source.page_id,
        source.ordinal,
        source.body_hash,
        decision.pass_key,
        decision.child,
        outcome_name(decision.outcome),
        decision.reason
    ])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            continue;
        }
        let Ok(stored) = serde_json::from_slice::<SelectionManifest>(&bytes) else {
            continue;
        };
        if !manifest_equivalent(&stored, manifest) {
            continue;
        }
        validate(tx, &stored)?;
        validate_rows(tx, &id, &stored)?;
        let expected = digest(
            &serde_json::to_vec(&(account_key, provider_key, MANIFEST_VERSION, &bytes))
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
        );
        if expected != id {
            continue;
        }
        return Ok(Some(id));
    }
    Ok(None)
}

fn validate(conn: &Connection, manifest: &SelectionManifest) -> Result<(), StateError> {
    if manifest.config_hash.len() != 64
        || !manifest.config_hash.bytes().all(|b| b.is_ascii_hexdigit())
        || manifest
            .profile
            .get("format")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
        || manifest
            .profile
            .get("coverage")
            .and_then(serde_json::Value::as_str)
            != Some("confirmed_sources_only")
        || manifest.sources.is_empty()
        || manifest.decisions.is_empty()
    {
        return invalid();
    }
    let scope: serde_json::Value = serde_json::from_str(&manifest.scope)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let Some(source_zone) = scope.get("zone") else {
        return invalid();
    };
    let zone = &manifest.zone;
    if scope.get("database").and_then(serde_json::Value::as_str) != Some("private")
        || zone.get("zoneName") != source_zone.get("zoneName")
        || zone.get("ownerRecordName") != source_zone.get("ownerRecordName")
        || zone
            .get("ownerRecordName")
            .and_then(serde_json::Value::as_str)
            != Some("_defaultOwner")
    {
        return invalid();
    }
    let mut sources = HashSet::new();
    for source in &manifest.sources {
        if !sources.insert((source.page_id, source.ordinal)) {
            return invalid();
        }
        let page = load_page(conn, CapturedPageId(source.page_id), MAX_MANIFEST_BYTES)?;
        let ordinal = usize::try_from(source.ordinal)
            .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        if page.page.scope != manifest.scope
            || page.observation_hash != source.body_hash
            || page.page.identities.get(ordinal).is_none()
        {
            return invalid();
        }
        let validated = crate::icloud::photos::catalog_observed_page(
            page.page.body.clone(),
            &manifest.scope,
            &page.page.request_cursor,
        )
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
        if validated != page.page {
            return invalid();
        }
        let indexed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM provider_catalog_records c JOIN provider_catalog_pages p ON p.page_id=c.page_id WHERE c.page_id=?1 AND c.ordinal=?2 AND p.projector_version=1 AND p.body_hash=?3)",
            params![source.page_id, source.ordinal, source.body_hash], |r| r.get(0),
        )?;
        if !indexed {
            return invalid();
        }
    }
    let mut decisions = HashSet::new();
    for decision in &manifest.decisions {
        if decision.pass_key.trim().is_empty()
            || decision.child.trim().is_empty()
            || !decisions.insert((&decision.pass_key, &decision.child))
            || decision.destinations.len() > 32
            || (decision.outcome != SelectionOutcome::Selected && !decision.destinations.is_empty())
            || (decision.outcome == SelectionOutcome::Selected && !decision.reason.is_empty())
            || (decision.outcome != SelectionOutcome::Selected && decision.reason.is_empty())
        {
            return invalid();
        }
        let photo = match (&decision.confirmation, &decision.master) {
            (Some(body), Some(master)) if body.len() <= MAX_MANIFEST_BYTES => Some(
                crate::icloud::photos::current_asset(body, &decision.child, master, zone)
                    .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
            ),
            (None, None) if decision.outcome == SelectionOutcome::Deferred => None,
            _ => return invalid(),
        };
        let mut destinations = HashSet::new();
        for destination in &decision.destinations {
            let Some(version) = VersionSizeKey::from_str(&destination.version_size) else {
                return invalid();
            };
            let path = destination.path.to_path();
            if version.as_str() != destination.version_size
                || !path.is_absolute()
                || path.file_name().is_none()
                || !destinations.insert((&destination.version_size, destination.path.key()?))
            {
                return invalid();
            }
            let Some(photo) = &photo else {
                return invalid();
            };
            if !photo.versions().iter().any(|(key, resource)| {
                let provider_version = VersionSizeKey::from(*key);
                let matches = provider_version == version
                    || matches!(
                        (provider_version, version),
                        (VersionSizeKey::Original, VersionSizeKey::Alternative)
                            | (VersionSizeKey::Alternative, VersionSizeKey::Original)
                    );
                matches
                    && resource.checksum.as_ref() == destination.checksum
                    && resource.size == destination.size
                    && photo
                        .metadata_arc(provider_version)
                        .metadata_hash
                        .as_deref()
                        == Some(destination.metadata_hash.as_str())
            }) {
                return invalid();
            }
        }
    }
    Ok(())
}

impl SqliteStateDb {
    /// Atomic shadow evidence only. An exact replay is possible at capacity.
    pub(crate) async fn capture_selection_shadow(
        &self,
        owner: account::AccountOwner,
        manifest: SelectionManifest,
        capacity: u64,
    ) -> Result<String, StateError> {
        self.with_conn_mut("capturing selection shadow", move |conn| {
            let bytes = serde_json::to_vec(&manifest)
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
            if bytes.len() > MAX_MANIFEST_BYTES { return Err(StateError::ProviderSelectionFull) }
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            validate(&tx, &manifest)?;
            let (account_key, provider_key): (String, String) = tx.query_row(
                "SELECT account_key,provider_key FROM account_owner WHERE singleton=1", [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let id = digest(&serde_json::to_vec(&(&account_key, &provider_key, MANIFEST_VERSION, &bytes))
                .map_err(|_invalid| StateError::ProviderSelectionInvalid)?);
            let existing: Option<(String, String, i64, Vec<u8>)> = tx.query_row(
                "SELECT account_key,provider_key,format,manifest FROM provider_selection_generations WHERE id=?1",
                [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            ).optional()?;
            if let Some(existing) = existing {
                if existing != (account_key, provider_key, MANIFEST_VERSION, bytes) { return invalid() }
                validate_rows(&tx, &id, &manifest)?;
                tx.commit()?;
                return Ok(id);
            }
            if let Some(equivalent) =
                equivalent_generation(&tx, &manifest, &account_key, &provider_key)?
            {
                tx.commit()?;
                return Ok(equivalent);
            }
            let mut charge = bytes.len() + id.len() + account_key.len() + provider_key.len()
                + manifest.scope.len() + manifest.config_hash.len();
            for source in &manifest.sources { charge += id.len() + source.body_hash.len() + 16; }
            for decision in &manifest.decisions {
                charge += id.len() + decision.pass_key.len() + decision.child.len()
                    + decision.reason.len() + outcome_name(decision.outcome).len();
                for destination in &decision.destinations {
                    charge += id.len() + decision.pass_key.len() + decision.child.len()
                        + destination.version_size.len() + destination.path.key()?.len()
                        + destination.checksum.len() + destination.metadata_hash.len() + 8;
                }
            }
            let charge = u64::try_from(charge).map_err(|_invalid| StateError::ProviderSelectionFull)?;
            let used: i64 = tx.query_row("SELECT COALESCE(SUM(charged_bytes),0) FROM provider_selection_generations", [], |r| r.get(0))?;
            if charge > capacity.saturating_sub(u64::try_from(used).map_err(|_invalid| StateError::ProviderSelectionInvalid)?) {
                return Err(StateError::ProviderSelectionFull);
            }
            tx.execute("INSERT INTO provider_selection_generations(id,account_key,provider_key,format,scope,config_hash,manifest,charged_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![id,account_key,provider_key,MANIFEST_VERSION,manifest.scope,manifest.config_hash,bytes,i64::try_from(charge).map_err(|_invalid| StateError::ProviderSelectionFull)?])?;
            for source in &manifest.sources {
                tx.execute("INSERT INTO provider_selection_sources(generation,page_id,ordinal,body_hash) VALUES(?1,?2,?3,?4)", params![id,source.page_id,source.ordinal,source.body_hash])?;
            }
            for decision in &manifest.decisions {
                let outcome = outcome_name(decision.outcome);
                tx.execute("INSERT INTO provider_selection_decisions(generation,pass_key,child,outcome,reason) VALUES(?1,?2,?3,?4,?5)", params![id,decision.pass_key,decision.child,outcome,decision.reason])?;
                for destination in &decision.destinations {
                    tx.execute("INSERT INTO provider_selection_destinations(generation,pass_key,child,version_size,path,checksum,size_bytes,metadata_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![id,decision.pass_key,decision.child,destination.version_size,destination.path.key()?,destination.checksum,i64::try_from(destination.size).map_err(|_invalid| StateError::ProviderSelectionInvalid)?,destination.metadata_hash])?;
                }
            }
            tx.commit()?;
            Ok(id)
        }).await
    }

    /// Recheck owner, original sources, confirmations and normalized rows.
    #[cfg(test)]
    pub(crate) async fn replay_selection_shadow(
        &self,
        owner: account::AccountOwner,
        id: String,
    ) -> Result<SelectionManifest, StateError> {
        self.with_conn("replaying selection shadow", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let manifest = load_shadow(&tx, &owner, &id)?;
            tx.commit()?;
            Ok(manifest)
        })
        .await
    }

    /// Prove that a retained shadow is strictly provider-equivalent to the
    /// current planning manifest before its generation may be reused. The
    /// current manifest was already validated by capture; this second owner
    /// check revalidates the retained representative and every non-confirmation
    /// field without replacing its original bytes.
    pub(crate) async fn verify_selection_shadow_equivalent(
        &self,
        owner: account::AccountOwner,
        id: String,
        current: SelectionManifest,
    ) -> Result<(), StateError> {
        self.with_conn("verifying equivalent selection shadow", move |conn| {
            let tx = conn.unchecked_transaction()?;
            let stored = load_shadow(&tx, &owner, &id)?;
            if !manifest_equivalent(&stored, &current) {
                return invalid();
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

fn load_shadow(
    conn: &Connection,
    owner: &account::AccountOwner,
    id: &str,
) -> Result<SelectionManifest, StateError> {
    account::validate_authenticated(conn, owner)?;
    let length: i64 = conn.query_row(
        "SELECT length(manifest) FROM provider_selection_generations WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if usize::try_from(length).map_or(true, |n| n > MAX_MANIFEST_BYTES) {
        return invalid();
    }
    let (account_key, provider_key, format, scope, config_hash, bytes): (String,String,i64,String,String,Vec<u8>) = conn.query_row("SELECT account_key,provider_key,format,scope,config_hash,manifest FROM provider_selection_generations WHERE id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    let actual: (String, String) = conn.query_row(
        "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if actual != (account_key.clone(), provider_key.clone())
        || format != MANIFEST_VERSION
        || id
            != digest(
                &serde_json::to_vec(&(&account_key, &provider_key, format, &bytes))
                    .map_err(|_invalid| StateError::ProviderSelectionInvalid)?,
            )
    {
        return invalid();
    }
    let manifest: SelectionManifest =
        serde_json::from_slice(&bytes).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    if scope != manifest.scope || config_hash != manifest.config_hash {
        return invalid();
    }
    validate(conn, &manifest)?;
    validate_rows(conn, id, &manifest)?;
    Ok(manifest)
}

fn outcome_name(outcome: SelectionOutcome) -> &'static str {
    match outcome {
        SelectionOutcome::Selected => "selected",
        SelectionOutcome::Excluded => "excluded",
        SelectionOutcome::Deferred => "deferred",
    }
}

fn validate_rows(
    conn: &Connection,
    id: &str,
    manifest: &SelectionManifest,
) -> Result<(), StateError> {
    for (table, expected) in [
        ("provider_selection_sources", manifest.sources.len()),
        ("provider_selection_decisions", manifest.decisions.len()),
        (
            "provider_selection_destinations",
            manifest
                .decisions
                .iter()
                .map(|d| d.destinations.len())
                .sum(),
        ),
    ] {
        let count: i64 = conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE generation=?1"),
            [id],
            |r| r.get(0),
        )?;
        if usize::try_from(count).ok() != Some(expected) {
            return invalid();
        }
    }
    for source in &manifest.sources {
        let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_selection_sources WHERE generation=?1 AND page_id=?2 AND ordinal=?3 AND body_hash=?4)", params![id,source.page_id,source.ordinal,source.body_hash], |r|r.get(0))?;
        if !valid {
            return invalid();
        }
    }
    for decision in &manifest.decisions {
        let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_selection_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3 AND outcome=?4 AND reason=?5)", params![id,decision.pass_key,decision.child,outcome_name(decision.outcome),decision.reason], |r|r.get(0))?;
        if !valid {
            return invalid();
        }
        for destination in &decision.destinations {
            let valid: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_selection_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND checksum=?6 AND size_bytes=?7 AND metadata_hash=?8)", params![id,decision.pass_key,decision.child,destination.version_size,destination.path.key()?,destination.checksum,i64::try_from(destination.size).map_err(|_invalid|StateError::ProviderSelectionInvalid)?,destination.metadata_hash], |r|r.get(0))?;
            if !valid {
                return invalid();
            }
        }
    }
    Ok(())
}
