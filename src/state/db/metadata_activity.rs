//! Durable prepublication owner evidence. It does not authorize a file write.
use super::{MetadataRewriteQueue, PendingMetadataRewrite, SqliteStateDb, account};
use crate::state::error::StateError;
#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::{TransactionBehavior, params};

// The activity owner currently binds an embedded media component only.
const SIDECAR_COMPONENT_FLAG: u8 = 1 << 5;

pub(crate) const DDL: &str = "CREATE TABLE metadata_activity_prerequisite (
 operation_id TEXT PRIMARY KEY, account_key TEXT NOT NULL, provider_key TEXT NOT NULL,
 library TEXT NOT NULL, binding BLOB NOT NULL);
 CREATE INDEX metadata_activity_prerequisite_library ON metadata_activity_prerequisite(account_key,provider_key,library);
 CREATE TABLE metadata_activity_prepared_prerequisite (operation_id TEXT PRIMARY KEY, binding BLOB NOT NULL);";

// Version 42/43 records never retained an admission inode. They remain unknown;
// reopening a pathname today cannot reconstruct the inode originally admitted.
pub(crate) const IDENTITY_INDEX_MIGRATION: &str = "
 ALTER TABLE metadata_activity_prerequisite ADD COLUMN target_key TEXT;
 ALTER TABLE metadata_activity_prerequisite ADD COLUMN lookup_state INTEGER NOT NULL DEFAULT 0
   CHECK (CASE lookup_state
       WHEN 0 THEN target_key IS NULL
       WHEN 1 THEN CASE WHEN target_key IS NOT NULL AND length(target_key)>0
           AND json_valid(target_key) AND json_valid(binding)
           THEN COALESCE(json_type(binding,'$.path')='object'
                AND json_extract(binding,'$.path')=target_key
                AND json_type(binding,'$.version')='integer'
                AND json_extract(binding,'$.version')=2
                AND json_type(binding,'$.asset')='object'
                AND json_type(binding,'$.asset.id')='text'
                AND length(json_extract(binding,'$.asset.id'))>0
                AND json_type(binding,'$.asset.checksum')='text'
                AND length(json_extract(binding,'$.asset.checksum'))>0
                AND json_type(binding,'$.asset.library')='text'
                AND json_extract(binding,'$.asset.library')=library
                AND json_type(binding,'$.queue')='text'
                AND json_extract(binding,'$.queue')='ordinary'
                AND json_type(binding,'$.component')='text'
                AND json_extract(binding,'$.component')='embedded'
                AND json_type(binding,'$.capture')='null'
                AND json_type(binding,'$.flags')='integer'
                AND json_extract(binding,'$.flags') BETWEEN 0 AND 63
                AND (json_extract(binding,'$.flags') & 32)=0,0)
           ELSE 0 END
       ELSE 0 END);
 CREATE INDEX metadata_activity_lookup_unknown ON metadata_activity_prerequisite(account_key,provider_key,library,lookup_state);
 CREATE INDEX metadata_activity_lookup_target ON metadata_activity_prerequisite(account_key,provider_key,library,target_key,lookup_state);";

/// An account-owned exact work binding, never a publication or deletion permit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataActivity {
    operation_id: String,
    account_key: String,
    provider_key: String,
    library: String,
    binding: Vec<u8>,
}

/// In-process continuation capability issued only by the authenticated owner
/// after activity insertion commits. It is deliberately not Clone or serializable;
/// a listed record is evidence, not a capability. Loss on restart holds the work.
/// This capability does not authorize publication, cleanup, or settlement.
pub struct ActivityAdmission {
    activity: MetadataActivity,
    // A reopened or copied database cannot reconstruct a live capability.
    origin: std::sync::Weak<std::sync::Mutex<rusqlite::Connection>>,
    held: Option<std::sync::Arc<HeldFile>>,
}

// Live evidence is never reconstructed from SQLite after capability loss.
// Another connection, process, restart, or poisoned registry yields unknown.
struct LiveActivity {
    origin: std::sync::Weak<std::sync::Mutex<rusqlite::Connection>>,
    file: std::sync::Weak<HeldFile>,
}
static LIVE_ACTIVITIES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, LiveActivity>>,
> = std::sync::OnceLock::new();
fn live_activities() -> &'static std::sync::Mutex<std::collections::HashMap<String, LiveActivity>> {
    LIVE_ACTIVITIES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
fn register_live(
    active: &MetadataActivity,
    origin: &std::sync::Weak<std::sync::Mutex<rusqlite::Connection>>,
    file: &std::sync::Arc<HeldFile>,
) -> Result<(), StateError> {
    let mut live = live_activities().lock().map_err(|_poisoned| invalid())?;
    if live.contains_key(&active.operation_id) {
        return Err(invalid());
    }
    live.insert(
        active.operation_id.clone(),
        LiveActivity {
            origin: origin.clone(),
            file: std::sync::Arc::downgrade(file),
        },
    );
    Ok(())
}
impl Drop for ActivityAdmission {
    fn drop(&mut self) {
        if self.held.is_some()
            && let Ok(mut live) = live_activities().lock()
        {
            live.remove(&self.activity.operation_id);
        }
    }
}

/// A selected, rating-only no-write observation retaining the exact file
/// opened before its durable activity was inserted. This is not authority to
/// publish, finalize metadata, clear work, or retire the activity.
#[allow(
    dead_code,
    reason = "the retained capability is not connected to completion until indexed conflict fencing exists"
)]
pub struct RatingNoWriteAdmission {
    activity: ActivityAdmission,
    file: std::sync::Arc<HeldFile>,
    lease: std::sync::Arc<std::sync::Mutex<super::metadata_identity::IdentityActivity>>,
}

#[cfg(test)]
impl RatingNoWriteAdmission {
    pub(crate) fn test_validate_file(&self) -> Result<(), StateError> {
        self.file.validate()
    }

    pub(crate) fn test_operation_id(&self) -> &str {
        &self.activity.activity.operation_id
    }

    pub(crate) fn test_admission(&self) -> &ActivityAdmission {
        &self.activity
    }
}

impl std::fmt::Debug for ActivityAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityAdmission").finish_non_exhaustive()
    }
}

impl ActivityAdmission {
    #[cfg(test)]
    pub(crate) fn test_operation_id(&self) -> &str {
        &self.activity.operation_id
    }

    fn belongs_to(&self, db: &SqliteStateDb) -> bool {
        std::sync::Weak::ptr_eq(&self.origin, &std::sync::Arc::downgrade(&db.conn))
    }
}

fn invalid() -> StateError {
    StateError::ProviderWorkConflict
}

fn binding(
    pending: &PendingMetadataRewrite,
    queue: MetadataRewriteQueue,
    flags: u8,
) -> Result<Vec<u8>, StateError> {
    let asset = &pending.asset;
    let path = asset.local_path.as_deref().ok_or_else(invalid)?;
    let selection = pending.selection_receipt.as_ref().map(|r| {
        serde_json::json!({
            "generation":r.generation,"pass":r.pass_key,"child":r.child,"grouping":r.grouping_hash,
            "frozen":r.frozen_metadata_hash,"frozen_groupings":r.frozen_groupings,"flags":r.metadata_flags,"prepared":r.prepared
        })
    });
    let capture=pending.capture_repair_receipt.as_ref().map(|r| match r {
        super::CaptureRepairReceipt::Pending{metadata_hash}=>serde_json::json!({"state":"pending","hash":metadata_hash}),
        super::CaptureRepairReceipt::Prepared{metadata_hash,output_checksum,output_size}=>serde_json::json!({"state":"prepared","hash":metadata_hash,"checksum":output_checksum,"size":output_size})
    });
    let mut frozen_asset = asset.clone();
    frozen_asset.local_path = None;
    serde_json::to_vec(&serde_json::json!({"version":2,"asset":frozen_asset,"path":super::provider_selection::SelectionPath::from_path(path),
        "source_checksum":pending.source_checksum,"selection":selection,"capture":capture,
        "queue": if queue==MetadataRewriteQueue::Ordinary {"ordinary"} else {"capture"},"flags":flags,"component":"embedded"}))
        .map_err(|_source| invalid())
}

fn validate_live(
    conn: &rusqlite::Connection,
    pending: &PendingMetadataRewrite,
    queue: MetadataRewriteQueue,
    flags: u8,
) -> Result<(), StateError> {
    if queue != MetadataRewriteQueue::Ordinary
        || pending.capture_repair_receipt.is_some()
        || flags & !0x3f != 0
        || flags & SIDECAR_COMPONENT_FLAG != 0
        || pending
            .selection_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.metadata_flags != flags)
    {
        return Err(invalid());
    }
    super::metadata::validate_activity_selected_work(conn, pending)?;
    let asset = &pending.asset;
    let path = asset.local_path.as_deref().ok_or_else(invalid)?;
    if pending.selection_receipt.is_none() {
        let source = super::asset_writes::metadata_rewrite_source_sql();
        let exists: bool=conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM ({source}) WHERE library=?1 AND id=?2 AND version_size=?3 AND checksum=?4 AND metadata_hash IS ?5 AND local_path IS ?6 AND local_checksum IS ?7 AND status='downloaded' AND is_deleted=0 AND ((?8=0 AND metadata_write_failed_at IS NOT NULL) OR (?8=1 AND capture_repair_metadata_hash IS NOT NULL)))"),params![asset.library,asset.id,asset.version_size.as_str(),asset.checksum,asset.metadata.metadata_hash,path.to_str(),asset.local_checksum,i64::from(queue==MetadataRewriteQueue::CaptureRepair)],|row|row.get(0))?;
        if !exists {
            return Err(invalid());
        }
    }
    Ok(())
}

/// A no-follow open file and parent kept through the evidence transaction.
struct HeldFile {
    path: crate::fs_util::ConfinedPath,
    file: std::fs::File,
    identity: crate::fs_util::FileIdentity,
    size: u64,
    sha256: String,
}

impl HeldFile {
    fn open(root: &std::path::Path, name: &std::path::Path) -> Result<Self, StateError> {
        let path = crate::fs_util::ConfinedPath::open(
            root,
            name,
            crate::fs_util::ConfinedParents::Existing,
        )
        .map_err(|_source| invalid())?;
        let file = path.open_regular().map_err(|_source| invalid())?;
        let identity = crate::fs_util::file_identity(&file).map_err(|_source| invalid())?;
        let (size, sha256) = Self::read(&path, identity)?;
        Ok(Self {
            path,
            file,
            identity,
            size,
            sha256,
        })
    }
    fn read(
        path: &crate::fs_util::ConfinedPath,
        identity: crate::fs_util::FileIdentity,
    ) -> Result<(u64, String), StateError> {
        use sha2::Digest;
        use std::io::Read;
        let mut file = path
            .validate_identity(identity)
            .map_err(|_source| invalid())?;
        let mut digest = sha2::Sha256::new();
        let mut size = 0u64;
        let mut buf = [0u8; 65536];
        loop {
            let n = file.read(&mut buf).map_err(|_source| invalid())?;
            if n == 0 {
                break;
            }
            digest.update(buf.get(..n).ok_or_else(invalid)?);
            size = size.checked_add(n as u64).ok_or_else(invalid)?;
        }
        path.validate_identity(identity)
            .map_err(|_source| invalid())?;
        Ok((size, data_encoding::HEXLOWER.encode(&digest.finalize())))
    }
    fn validate(&self) -> Result<(), StateError> {
        if crate::fs_util::file_identity(&self.file).map_err(|_source| invalid())? != self.identity
            || Self::read(&self.path, self.identity)? != (self.size, self.sha256.clone())
        {
            return Err(invalid());
        }
        Ok(())
    }
    fn lookup_target(&self) -> Result<String, StateError> {
        // The held open file is authoritative only while retained. Never use
        // a durable inode inequality as evidence that paths are independent.
        if crate::fs_util::file_link_count(&self.file).map_err(|_source| invalid())? != 1 {
            return Err(invalid());
        }
        super::provider_selection::SelectionPath::from_path(self.path.path()).key()
    }
    #[cfg(test)]
    fn evidence(&self) -> serde_json::Value {
        serde_json::json!({"path":super::provider_selection::SelectionPath::from_path(self.path.path()),"identity":self.identity,"size":self.size,"sha256":self.sha256})
    }
}

// Exact path lookup is a possible-conflict index, not proof of filesystem
// namespace independence or permission to mutate. Unknown legacy rows hold.
fn indexed_conflict(
    conn: &rusqlite::Connection,
    account: &str,
    provider: &str,
    library: &str,
    target: &str,
) -> Result<bool, StateError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM metadata_activity_prerequisite WHERE account_key=?1 AND provider_key=?2 AND library=?3 AND lookup_state=0) OR EXISTS(SELECT 1 FROM metadata_activity_prerequisite WHERE account_key=?1 AND provider_key=?2 AND library=?3 AND target_key=?4 AND lookup_state=1)",
        params![account, provider, library, target], |row| row.get(0),
    )?)
}

// The bounded owner check compares actual confined files for differently
// spelled paths. An unclassifiable record or more than 256 candidates holds
// this operation; it never treats stale inode inequality as durable proof.
fn check_live_competitors(
    conn: &rusqlite::Connection,
    owner: &account::AccountOwner,
    pending: &PendingMetadataRewrite,
    candidate: &HeldFile,
    origin: &std::sync::Weak<std::sync::Mutex<rusqlite::Connection>>,
    except: Option<&str>,
) -> Result<(), StateError> {
    let (account, provider) = owner.authenticated_keys()?;
    let mut stmt = conn.prepare("SELECT operation_id,binding FROM metadata_activity_prerequisite WHERE account_key=?1 AND provider_key=?2 AND library=?3 ORDER BY operation_id LIMIT 257")?;
    let rows = stmt
        .query_map(params![account, provider, pending.asset.library], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > 256 {
        return Err(invalid());
    }
    for (operation_id, binding) in rows {
        if except == Some(operation_id.as_str()) {
            continue;
        }
        let active = MetadataActivity {
            operation_id,
            account_key: account.to_owned(),
            provider_key: provider.to_owned(),
            library: pending.asset.library.to_string(),
            binding,
        };
        let saved = active.validated_binding()?;
        if saved.path.to_path() == candidate.path.path() {
            return Err(invalid());
        }
        let other = {
            let live = live_activities().lock().map_err(|_poisoned| invalid())?;
            let entry = live.get(&active.operation_id).ok_or_else(invalid)?;
            if !std::sync::Weak::ptr_eq(&entry.origin, origin) {
                return Err(invalid());
            }
            entry.file.upgrade().ok_or_else(invalid)?
        };
        // Compare simultaneously retained capabilities, never a reopened
        // historical name or a persisted inode across restart/remount.
        other.validate()?;
        other.lookup_target()?;
        candidate.validate()?;
        candidate.lookup_target()?;
        if other.identity == candidate.identity {
            return Err(invalid());
        }
        other.validate()?;
        candidate.validate()?;
    }
    Ok(())
}

/// Retained owner proof for an unchanged, explicitly selected rating request.
pub(super) struct RatingNoWriteGuard {
    active: MetadataActivity,
    file: std::sync::Arc<HeldFile>,
    owner: account::AccountOwner,
    origin: std::sync::Weak<std::sync::Mutex<rusqlite::Connection>>,
    current_flags: u8,
    lease: std::sync::Arc<std::sync::Mutex<super::metadata_identity::IdentityActivity>>,
}

impl RatingNoWriteGuard {
    pub(super) fn validate(
        &self,
        conn: &rusqlite::Connection,
        pending: &PendingMetadataRewrite,
    ) -> Result<(), StateError> {
        account::validate_authenticated(conn, &self.owner)?;
        if !self
            .lease
            .lock()
            .map_err(|_| invalid())?
            .validates_observer(&self.file.file)
        {
            return Err(invalid());
        }
        if self.current_flags & 2 == 0
            || pending.capture_repair_receipt.is_some()
            || pending.asset.metadata.rating.is_some()
            || pending.asset.metadata.is_favorite
        {
            return Err(invalid());
        }
        let receipt = pending.selection_receipt.as_ref().ok_or_else(invalid)?;
        if receipt.metadata_flags != 2 || receipt.prepared.is_some() {
            return Err(invalid());
        }
        self.file.validate()?;
        let target = self.file.lookup_target()?;
        if pending.asset.local_checksum.as_deref() != Some(self.file.sha256.as_str())
            || binding(pending, MetadataRewriteQueue::Ordinary, 2)? != self.active.binding
        {
            return Err(invalid());
        }
        validate_live(conn, pending, MetadataRewriteQueue::Ordinary, 2)?;
        super::provider_generations::validate_explicit_rating_refresh(conn, pending)?;
        let (account, provider) = self.owner.authenticated_keys()?;
        let exact: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM metadata_activity_prerequisite WHERE operation_id=?1 AND account_key=?2 AND provider_key=?3 AND library=?4 AND binding=?5 AND lookup_state=1 AND target_key=?6 AND NOT EXISTS(SELECT 1 FROM metadata_activity_prepared_prerequisite WHERE operation_id=?1))", params![self.active.operation_id,account,provider,self.active.library,self.active.binding,target], |r|r.get(0))?;
        if !exact {
            return Err(invalid());
        }
        if receipt.frozen_metadata_hash.is_none() {
            let absent: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND metadata_hash IS ?4 AND rating IS NULL AND is_favorite=0 AND is_deleted=0)",params![pending.asset.library,pending.asset.id,pending.asset.version_size.as_str(),pending.asset.metadata.metadata_hash],|r|r.get(0))?;
            if !absent {
                return Err(invalid());
            }
        }
        check_live_competitors(
            conn,
            &self.owner,
            pending,
            &self.file,
            &self.origin,
            Some(&self.active.operation_id),
        )?;
        self.file.validate()?;
        self.file.lookup_target()?;
        Ok(())
    }

    pub(super) fn retire(&self, conn: &rusqlite::Connection) -> Result<(), StateError> {
        let (account, provider) = self.owner.authenticated_keys()?;
        let target = self.file.lookup_target()?;
        let count = conn.execute("DELETE FROM metadata_activity_prerequisite WHERE operation_id=?1 AND account_key=?2 AND provider_key=?3 AND library=?4 AND binding=?5 AND lookup_state=1 AND target_key=?6 AND NOT EXISTS(SELECT 1 FROM metadata_activity_prepared_prerequisite WHERE operation_id=?1)", params![self.active.operation_id,account,provider,self.active.library,self.active.binding,target])?;
        if count != 1 {
            return Err(invalid());
        }
        self.file.validate()?;
        self.file.lookup_target()?;
        Ok(())
    }

    /// Revalidate the live owner binding after the exact receipt commits.
    pub(super) fn committed_binding(
        &self,
    ) -> Result<(super::metadata_identity::LeaseBinding, String), StateError> {
        let lease = self.lease.lock().map_err(|_| invalid())?;
        if !lease.validates_observer(&self.file.file) {
            return Err(invalid());
        }
        Ok((
            lease.commit_binding().ok_or_else(invalid)?,
            self.active.operation_id.clone(),
        ))
    }

    pub(super) fn committed(
        &self,
        seal: super::metadata::CommittedLease,
    ) -> Result<(), StateError> {
        let mut lease = self.lease.lock().map_err(|_| invalid())?;
        if !lease.validates_observer(&self.file.file)
            || !lease.settled_with(seal, &self.active.operation_id)
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// Reject-only classification; continuation still requires owner validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityPreflight {
    Independent,
    Conflicting,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenActivity {
    version: u8,
    asset: crate::state::types::AssetRecord,
    path: super::provider_selection::SelectionPath,
    source_checksum: Option<String>,
    selection: Option<FrozenSelection>,
    capture: Option<serde_json::Value>,
    queue: String,
    flags: u8,
    component: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenSelection {
    generation: String,
    pass: String,
    child: String,
    grouping: String,
    frozen: Option<String>,
    frozen_groupings: Option<(Vec<String>, Vec<String>)>,
    flags: u8,
    prepared: Option<(String, u64)>,
}

impl MetadataActivity {
    // Strictly decode and validate the entire supported operation binding
    // before its path may be promoted into the possible-conflict index.
    fn validated_binding(&self) -> Result<FrozenActivity, StateError> {
        let saved: FrozenActivity =
            serde_json::from_slice(&self.binding).map_err(|_invalid| invalid())?;
        if uuid::Uuid::parse_str(&self.operation_id).is_err()
            || saved.asset.id.is_empty()
            || saved.asset.checksum.is_empty()
            || saved.source_checksum.as_ref().is_some_and(|value| {
                value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            || saved.version != 2
            || saved.queue != "ordinary"
            || saved.component != "embedded"
            || saved.capture.is_some()
            || saved.flags & !0x3f != 0
            || saved.flags & SIDECAR_COMPONENT_FLAG != 0
            || saved.asset.local_path.is_some()
            || saved.asset.library.as_ref() != self.library
        {
            return Err(invalid());
        }
        if let Some(selected) = &saved.selection {
            if selected.generation.is_empty()
                || selected.pass.is_empty()
                || selected.child.is_empty()
                || selected.grouping.is_empty()
                || selected.flags != saved.flags
                || selected.prepared.as_ref().is_some_and(|(checksum, _)| {
                    checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return Err(invalid());
            }
            // Frozen fields remain part of the decoded binding; none alone is
            // a settlement witness or permission to replace a file.
            let _frozen = (
                &selected.frozen,
                &selected.frozen_groupings,
                &selected.prepared,
            );
        }
        Ok(saved)
    }

    /// Existing active records are never evidence of completed content. This
    /// read-only preflight cannot authorize a filesystem mutation.
    fn classify_preflight(
        &self,
        pending: &PendingMetadataRewrite,
        flags: u8,
        root: &std::path::Path,
    ) -> Result<ActivityPreflight, StateError> {
        let saved = self.validated_binding()?;
        let media = pending.asset.local_path.as_deref().ok_or_else(invalid)?;
        let saved_path = saved.path.to_path();
        let mut targets = Vec::new();
        // Flags other than XMP_SIDECAR are embedded metadata writes. Zero is
        // accepted by the owner-only synthetic control as the media component.
        if flags & !SIDECAR_COMPONENT_FLAG != 0 || flags == 0 {
            targets.push(media.to_owned());
        }
        if flags & SIDECAR_COMPONENT_FLAG != 0 {
            let mut name = media.as_os_str().to_os_string();
            name.push(".xmp");
            targets.push(std::path::PathBuf::from(name));
        }
        if targets.iter().any(|target| target == &saved_path) {
            return Ok(ActivityPreflight::Conflicting);
        }
        // Recorded files can live under an older root. All namespace and leaf
        // observations are no-follow and are preflight evidence only.
        let saved_root = saved_path.ancestors().last().ok_or_else(invalid)?;
        let old = crate::fs_util::ConfinedPath::open(
            saved_root,
            &saved_path,
            crate::fs_util::ConfinedParents::Existing,
        )
        .map_err(|_source| invalid())?;
        let old_file = old.open_regular().map_err(|_source| invalid())?;
        let old_id = crate::fs_util::file_identity(&old_file).map_err(|_source| invalid())?;
        old.validate_identity(old_id).map_err(|_source| invalid())?;
        for target in targets {
            let new = crate::fs_util::ConfinedPath::open(
                root,
                &target,
                crate::fs_util::ConfinedParents::Existing,
            )
            .map_err(|_source| invalid())?;
            if let Some(new_file) = new.open_optional_regular().map_err(|_source| invalid())? {
                let new_id =
                    crate::fs_util::file_identity(&new_file).map_err(|_source| invalid())?;
                new.validate_identity(new_id).map_err(|_source| invalid())?;
                if old_id == new_id {
                    return Ok(ActivityPreflight::Conflicting);
                }
            }
            old.validate_identity(old_id).map_err(|_source| invalid())?;
            new.validate_namespace().map_err(|_source| invalid())?;
        }
        let _source_checksum = &saved.source_checksum;
        Ok(ActivityPreflight::Independent)
    }
}

/// Exposed only through the existing authenticated state owner; no production
/// caller registers activity in this prerequisite.
#[async_trait::async_trait]
pub trait MetadataActivityOwner: Send + Sync {
    #[allow(
        dead_code,
        reason = "admission producer is deliberately not activated by the reject-only preflight"
    )]
    async fn admit_activity(
        &self,
        pending: &PendingMetadataRewrite,
        queue: MetadataRewriteQueue,
        flags: u8,
    ) -> Result<ActivityAdmission, StateError>;
    /// Bind the real selected rating-only no-op and its held pre-admission
    /// file. This alone is not a conflict fence or completion authority.
    #[allow(
        dead_code,
        reason = "production connection waits for indexed conflict fencing and exact owner completion"
    )]
    async fn admit_selected_rating_no_write(
        &self,
        pending: &PendingMetadataRewrite,
        root: &std::path::Path,
        current_flags: u8,
        current_queue: MetadataRewriteQueue,
    ) -> Result<RatingNoWriteAdmission, StateError>;
    async fn is_explicit_rating_refresh(
        &self,
        pending: &PendingMetadataRewrite,
    ) -> Result<bool, StateError>;
    async fn finish_selected_rating_no_write(
        &self,
        pending: &PendingMetadataRewrite,
        proof: &RatingNoWriteAdmission,
        current_flags: u8,
    ) -> Result<bool, StateError>;
    #[cfg(test)]
    async fn list_activity(&self, library: &str) -> Result<Vec<MetadataActivity>, StateError>;
    /// Reject-only preflight, not authority to publish or settle.
    async fn validate_conflicts(
        &self,
        pending: &PendingMetadataRewrite,
        queue: MetadataRewriteQueue,
        flags: u8,
        root: &std::path::Path,
        continuation: Option<&ActivityAdmission>,
    ) -> Result<(), StateError>;
    #[cfg(test)]
    async fn bind_prepared(
        &self,
        admission: &ActivityAdmission,
        pending: &PendingMetadataRewrite,
        flags: u8,
        root: &std::path::Path,
        stage: &std::path::Path,
    ) -> Result<(), StateError>;
    #[cfg(test)]
    async fn inspect_activity(
        &self,
        admission: &ActivityAdmission,
        pending: &PendingMetadataRewrite,
        queue: MetadataRewriteQueue,
        flags: u8,
        check: Box<dyn FnOnce() -> Result<(), StateError> + Send>,
    ) -> Result<(), StateError>;
}

#[async_trait::async_trait]
impl MetadataActivityOwner for SqliteStateDb {
    async fn admit_activity(
        &self,
        pending: &PendingMetadataRewrite,
        queue: MetadataRewriteQueue,
        flags: u8,
    ) -> Result<ActivityAdmission, StateError> {
        // Capture and other unverified forms require their own live proof owner.
        if queue != MetadataRewriteQueue::Ordinary
            || pending.capture_repair_receipt.is_some()
            || flags & !0x3f != 0
            || flags & SIDECAR_COMPONENT_FLAG != 0
            || pending
                .selection_receipt
                .as_ref()
                .is_some_and(|receipt| receipt.metadata_flags != flags)
        {
            return Err(invalid());
        }
        let owner = self
            .owner
            .clone()
            .ok_or(StateError::AccountIdentityUnavailable)?;
        let pending = pending.clone();
        let origin = std::sync::Arc::downgrade(&self.conn);
        self.with_conn_mut("admit metadata activity prerequisite", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            validate_live(&tx, &pending, queue, flags)?;
            let asset = &pending.asset;
            let (account, provider) = owner.authenticated_keys()?;
            let active = MetadataActivity {
                operation_id: uuid::Uuid::new_v4().to_string(),
                account_key: account.to_owned(),
                provider_key: provider.to_owned(),
                library: asset.library.to_string(),
                binding: binding(&pending, queue, flags)?,
            };
            tx.execute(
                "INSERT INTO metadata_activity_prerequisite(operation_id,account_key,provider_key,library,binding) VALUES(?1,?2,?3,?4,?5)",
                params![
                    active.operation_id,
                    active.account_key,
                    active.provider_key,
                    active.library,
                    active.binding
                ],
            )?;
            tx.commit()?;
            Ok(ActivityAdmission {
                activity: active,
                origin,
                held: None,
            })
        })
        .await
    }

    async fn admit_selected_rating_no_write(
        &self,
        pending: &PendingMetadataRewrite,
        root: &std::path::Path,
        current_flags: u8,
        current_queue: MetadataRewriteQueue,
    ) -> Result<RatingNoWriteAdmission, StateError> {
        const RATING_ONLY: u8 = 1 << 1;
        let receipt = pending.selection_receipt.as_ref().ok_or_else(invalid)?;
        if receipt.metadata_flags != RATING_ONLY
            || current_flags & RATING_ONLY == 0
            || current_queue != MetadataRewriteQueue::Ordinary
            || pending.activity_admission.is_some()
            || receipt.prepared.is_some()
            || pending.capture_repair_receipt.is_some()
            || pending.asset.metadata.rating.is_some()
            || pending.asset.metadata.is_favorite
        {
            return Err(invalid());
        }
        let owner = self
            .owner
            .clone()
            .ok_or(StateError::AccountIdentityUnavailable)?;
        let root = root.to_owned();
        let path = pending.asset.local_path.clone().ok_or_else(invalid)?;
        let file = tokio::task::spawn_blocking(move || HeldFile::open(&root, &path)).await??;
        if pending.asset.local_checksum.as_deref() != Some(file.sha256.as_str()) {
            return Err(invalid());
        }
        let pending = pending.clone();
        let origin = std::sync::Arc::downgrade(&self.conn);
        self.with_conn_mut("admit selected rating no-write evidence", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            validate_live(&tx, &pending, MetadataRewriteQueue::Ordinary, RATING_ONLY)?;
            super::provider_generations::validate_explicit_rating_refresh(&tx, &pending)?;
            let receipt = pending.selection_receipt.as_ref().ok_or_else(invalid)?;
            if receipt.frozen_metadata_hash.is_none() {
                // The row decoder maps invalid integer ratings to None. A
                // live-catalogue snapshot must prove SQL NULL directly.
                let absent: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND metadata_hash IS ?4 AND rating IS NULL AND is_favorite=0 AND is_deleted=0)",
                    params![pending.asset.library, pending.asset.id, pending.asset.version_size.as_str(), pending.asset.metadata.metadata_hash],
                    |row| row.get(0),
                )?;
                if !absent { return Err(invalid()); }
            }
            // The original inode comes from the retained open file, not a
            // later reopening of the recorded pathname. Unknown legacy rows
            // cannot prove disjointness and therefore conservatively hold.
            file.validate()?;
            let target_key = file.lookup_target()?;
            let (account, provider) = owner.authenticated_keys()?;
            if indexed_conflict(&tx, account, provider, &pending.asset.library, &target_key)? { return Err(invalid()); }
            check_live_competitors(&tx,&owner,&pending,&file,&origin,None)?;
            let active = MetadataActivity {
                operation_id: uuid::Uuid::new_v4().to_string(),
                account_key: account.to_owned(),
                provider_key: provider.to_owned(),
                library: pending.asset.library.to_string(),
                binding: binding(&pending, MetadataRewriteQueue::Ordinary, RATING_ONLY)?,
            };
            let decoded = active.validated_binding()?;
            if decoded.path.key()? != target_key { return Err(invalid()); }
            tx.execute(
                "INSERT INTO metadata_activity_prerequisite(operation_id,account_key,provider_key,library,binding,target_key,lookup_state) VALUES(?1,?2,?3,?4,?5,?6,1)",
                params![active.operation_id, active.account_key, active.provider_key, active.library, active.binding, target_key],
            )?;
            file.validate()?;
            tx.commit()?;
            let file = std::sync::Arc::new(file);
            let pin = std::sync::Arc::new(file.file.try_clone().map_err(|_| invalid())?);
            let lease = super::metadata_identity::IdentityActivity::observe(pin).ok_or_else(invalid)?;
            file.validate()?;
            file.lookup_target()?;
            register_live(&active, &origin, &file)?;
            Ok(RatingNoWriteAdmission {
                activity: ActivityAdmission { activity: active, origin, held: Some(std::sync::Arc::clone(&file)) },
                file,
                lease: std::sync::Arc::new(std::sync::Mutex::new(lease)),
            })
        }).await
    }

    async fn is_explicit_rating_refresh(
        &self,
        pending: &PendingMetadataRewrite,
    ) -> Result<bool, StateError> {
        let owner = self
            .owner
            .clone()
            .ok_or(StateError::AccountIdentityUnavailable)?;
        let pending = pending.clone();
        self.with_conn("classify selected explicit rating refresh", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            validate_live(conn, &pending, MetadataRewriteQueue::Ordinary, 2)?;
            super::provider_generations::is_explicit_rating_refresh(conn, &pending)
        })
        .await
    }

    async fn finish_selected_rating_no_write(
        &self,
        pending: &PendingMetadataRewrite,
        proof: &RatingNoWriteAdmission,
        current_flags: u8,
    ) -> Result<bool, StateError> {
        if !proof.activity.belongs_to(self) {
            return Err(invalid());
        }
        let owner = self
            .owner
            .clone()
            .ok_or(StateError::AccountIdentityUnavailable)?;
        let guard = RatingNoWriteGuard {
            active: proof.activity.activity.clone(),
            file: std::sync::Arc::clone(&proof.file),
            owner,
            origin: proof.activity.origin.clone(),
            current_flags,
            lease: std::sync::Arc::clone(&proof.lease),
        };
        self.finish_rating_no_write_guarded(pending, guard).await
    }

    async fn validate_conflicts(
        &self,
        pending: &PendingMetadataRewrite,
        queue: MetadataRewriteQueue,
        flags: u8,
        root: &std::path::Path,
        continuation: Option<&ActivityAdmission>,
    ) -> Result<(), StateError> {
        let Some(owner) = self
            .owner
            .clone()
            .filter(|owner| owner.authenticated_keys().is_ok())
        else {
            if continuation.is_some() {
                return Err(invalid());
            }
            return self
                .with_conn("check unowned metadata activity", |conn| {
                    let any: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM metadata_activity_prerequisite)",
                        [],
                        |row| row.get(0),
                    )?;
                    if any {
                        return Err(StateError::AccountIdentityUnavailable);
                    }
                    Ok(())
                })
                .await;
        };
        let pending = pending.clone();
        let root = root.to_owned();
        if continuation.is_some_and(|capability| !capability.belongs_to(self)) {
            return Err(invalid());
        }
        let continuation =
            continuation.map(|capability| (capability.activity.clone(), capability.held.clone()));
        self.with_conn_mut("validate metadata activity conflicts", move |conn| {
            // A preflight is read-only and is not a publication-time fence.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            account::validate_authenticated(&tx, &owner)?;
            let (account, provider) = owner.authenticated_keys()?;
            let mut statement = tx.prepare("SELECT operation_id,binding FROM metadata_activity_prerequisite WHERE account_key=?1 AND provider_key=?2 AND library=?3 ORDER BY operation_id")?;
            let rows = statement.query_map(params![account,provider,pending.asset.library], |row| Ok(MetadataActivity {
                operation_id: row.get(0)?, account_key: account.to_owned(), provider_key: provider.to_owned(),
                library: pending.asset.library.to_string(), binding: row.get(1)?,
            }))?.collect::<Result<Vec<_>,_>>()?;
            drop(statement);
            // Preserve the existing no-relevant-record path, without filesystem
            // probes, record creation, publication, or settlement.
            if rows.is_empty() {
                if continuation.is_some() { return Err(invalid()); }
                tx.commit()?;
                return Ok(());
            }
            let mut continuation_seen = false;
            for active in rows {
                if continuation.as_ref().is_some_and(|(claimed, _)| claimed == &active) {
                    let held = continuation.as_ref().and_then(|(_, held)| held.as_ref()).ok_or_else(invalid)?;
                    held.validate()?;
                    // Recheck live link count at the continuation boundary;
                    // an admission-time single-link observation can go stale.
                    held.lookup_target()?;
                    if held.path.path() != pending.asset.local_path.as_deref().ok_or_else(invalid)? || binding(&pending,queue,flags)? != active.binding { return Err(invalid()); }
                    validate_live(&tx,&pending,queue,flags)?;
                    // A prepared row without an operation-bound settlement
                    // witness remains ambiguous even to the original owner.
                    let prepared: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM metadata_activity_prepared_prerequisite WHERE operation_id=?1)", [&active.operation_id], |row| row.get(0))?;
                    if prepared { return Err(invalid()); }
                    continuation_seen = true;
                    continue;
                }
                if active.classify_preflight(&pending,flags,&root)? == ActivityPreflight::Conflicting {
                    return Err(invalid());
                }
            }
            if continuation.is_some() && !continuation_seen { return Err(invalid()); }
            tx.commit()?;
            Ok(())
        }).await
    }

    #[cfg(test)]
    async fn bind_prepared(
        &self,
        admission: &ActivityAdmission,
        pending: &PendingMetadataRewrite,
        flags: u8,
        root: &std::path::Path,
        stage: &std::path::Path,
    ) -> Result<(), StateError> {
        let owner = self
            .owner
            .clone()
            .ok_or(StateError::AccountIdentityUnavailable)?;
        if !admission.belongs_to(self) {
            return Err(invalid());
        }
        let active = admission.activity.clone();
        let pending = pending.clone();
        let root = root.to_owned();
        let stage = stage.to_owned();
        self.with_conn_mut("bind metadata prepared observation", move |conn| {
            let original=pending.asset.local_path.as_deref().ok_or_else(invalid)?;
            if original==stage { return Err(invalid()); }
            let original=HeldFile::open(&root,original)?;
            let prepared=HeldFile::open(&root,&stage)?;
            if original.identity == prepared.identity {return Err(invalid());}
            if pending.asset.local_checksum.as_deref()!=Some(original.sha256.as_str()) {return Err(invalid());}
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let (account,provider)=owner.authenticated_keys()?;
            if account!=active.account_key || provider!=active.provider_key || binding(&pending,MetadataRewriteQueue::Ordinary,flags)?!=active.binding {return Err(invalid());}
            let stored:Option<Vec<u8>>=tx.query_row("SELECT binding FROM metadata_activity_prerequisite WHERE operation_id=?1 AND account_key=?2 AND provider_key=?3 AND library=?4",params![active.operation_id,account,provider,active.library],|row|row.get(0)).optional()?;
            if stored.as_deref()!=Some(active.binding.as_slice()) {return Err(invalid());}
            validate_live(&tx,&pending,MetadataRewriteQueue::Ordinary,flags)?;
            original.validate()?;
            prepared.validate()?;
            let evidence=serde_json::to_vec(&serde_json::json!({"version":1,"operation":active.operation_id,"original":original.evidence(),"stage":prepared.evidence()})).map_err(|_source| invalid())?;
            let existing:Option<Vec<u8>>=tx.query_row("SELECT binding FROM metadata_activity_prepared_prerequisite WHERE operation_id=?1",[&active.operation_id],|row|row.get(0)).optional()?;
            match existing { Some(value) if value!=evidence => return Err(invalid()), Some(_) => {}, None => { tx.execute("INSERT INTO metadata_activity_prepared_prerequisite VALUES(?1,?2)",params![active.operation_id,evidence])?; } }
            original.validate()?;
            prepared.validate()?;
            tx.commit()?;
            Ok(())
        }).await
    }

    #[cfg(test)]
    async fn list_activity(&self, library: &str) -> Result<Vec<MetadataActivity>, StateError> {
        let owner = self
            .owner
            .clone()
            .ok_or(StateError::AccountIdentityUnavailable)?;
        let library = library.to_owned();
        self.with_conn("list metadata activity prerequisite", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let (account,provider)=owner.authenticated_keys()?;
            let mut statement=conn.prepare("SELECT operation_id,binding FROM metadata_activity_prerequisite WHERE account_key=?1 AND provider_key=?2 AND library=?3 ORDER BY operation_id")?;
            let rows=statement.query_map(params![account,provider,library],|row| Ok(MetadataActivity { operation_id:row.get(0)?,account_key:account.to_owned(),provider_key:provider.to_owned(),library:library.clone(),binding:row.get(1)? }))?;
            Ok(rows.collect::<Result<Vec<_>,_>>()?)
        }).await
    }

    #[cfg(test)]
    async fn inspect_activity(
        &self,
        admission: &ActivityAdmission,
        pending: &PendingMetadataRewrite,
        queue: MetadataRewriteQueue,
        flags: u8,
        check: Box<dyn FnOnce() -> Result<(), StateError> + Send>,
    ) -> Result<(), StateError> {
        let owner = self
            .owner
            .clone()
            .ok_or(StateError::AccountIdentityUnavailable)?;
        if !admission.belongs_to(self) {
            return Err(invalid());
        }
        let active = admission.activity.clone();
        let pending = pending.clone();
        self.with_conn_mut("inspect metadata activity prerequisite",move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx,&owner)?;
            let (account,provider)=owner.authenticated_keys()?;
            if account!=active.account_key || provider!=active.provider_key {return Err(invalid());}
            let stored:Option<Vec<u8>>=tx.query_row("SELECT binding FROM metadata_activity_prerequisite WHERE operation_id=?1 AND account_key=?2 AND provider_key=?3 AND library=?4",params![active.operation_id,account,provider,active.library],|row|row.get(0)).optional()?;
            if stored.as_deref()!=Some(active.binding.as_slice()) || binding(&pending,queue,flags)? != active.binding {return Err(invalid());}
            validate_live(&tx,&pending,queue,flags)?;
            check()?;
            tx.commit()?;
            Ok(())
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestAssetRecord;
    use std::sync::Arc;

    fn owner() -> account::AccountOwner {
        account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-provider"}}))
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn authenticated_activity_survives_cancel_and_reopen_without_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
        assert!(
            crate::state::db::MetadataRewriteStore::metadata_activity_owner(db.as_ref()).is_some()
        );
        let unowned = SqliteStateDb::open_in_memory().unwrap();
        assert!(unowned.list_activity("PrimarySync").await.is_err());
        use sha2::Digest;
        let media = dir.path().join("asset.jpg");
        let stage = dir.path().join("asset.stage");
        std::fs::write(&media, b"original bytes").unwrap();
        std::fs::write(&stage, b"prepared bytes").unwrap();
        let local = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(b"original bytes"));
        let asset = TestAssetRecord::new("asset")
            .checksum("provider-checksum")
            .build();
        db.upsert_seen(&asset).await.unwrap();
        db.mark_downloaded("PrimarySync", "asset", "original", &media, &local, None)
            .await
            .unwrap();
        db.record_metadata_write_failure("PrimarySync", "asset", "original")
            .await
            .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                Some(&["PrimarySync"]),
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        // Exercise the same store-role entrypoint used by the queued writer.
        crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
            db.as_ref(),
            &pending,
            MetadataRewriteQueue::Ordinary,
            0,
            dir.path(),
        )
        .await
        .unwrap();
        crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
            &unowned,
            &pending,
            MetadataRewriteQueue::Ordinary,
            0,
            dir.path(),
        )
        .await
        .unwrap();
        // No activity: checking creates no record and allows the prior path.
        db.validate_conflicts(
            &pending,
            MetadataRewriteQueue::Ordinary,
            0,
            dir.path(),
            None,
        )
        .await
        .unwrap();
        let mut wrong = pending.clone();
        wrong.asset.local_checksum = Some("not-local".to_owned());
        assert!(
            db.admit_activity(&wrong, MetadataRewriteQueue::Ordinary, 0)
                .await
                .is_err()
        );
        let admission = Arc::new(
            db.admit_activity(&pending, MetadataRewriteQueue::Ordinary, 0)
                .await
                .unwrap(),
        );
        let active = admission.activity.clone();
        assert!(
            crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
                db.as_ref(),
                &pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
            )
            .await
            .is_err()
        );
        let mut own_pending = pending.clone();
        own_pending.activity_admission = Some(Arc::clone(&admission));
        // A legacy admission has no retained file capability and cannot
        // authorize same-operation continuation, even on the same connection.
        assert!(
            crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
                db.as_ref(),
                &own_pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
            )
            .await
            .is_err()
        );
        assert!(
            db.validate_conflicts(
                &pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
                None
            )
            .await
            .is_err()
        );
        assert!(
            db.validate_conflicts(
                &pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
                Some(&admission),
            )
            .await
            .is_err()
        );
        assert!(
            db.validate_conflicts(
                &wrong,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
                Some(&admission)
            )
            .await
            .is_err()
        );
        let other_db = SqliteStateDb::open_owned(&dir.path().join("other.db"), &owner())
            .await
            .unwrap();
        // A genuine token from a different database must not turn the empty
        // record case into a same-operation continuation.
        assert!(
            other_db
                .validate_conflicts(
                    &pending,
                    MetadataRewriteQueue::Ordinary,
                    0,
                    dir.path(),
                    Some(&admission)
                )
                .await
                .is_err()
        );
        #[cfg(unix)]
        {
            let alias = dir.path().join("original-alias");
            std::fs::hard_link(&media, &alias).unwrap();
            assert!(
                db.bind_prepared(&admission, &pending, 0, dir.path(), &alias)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&alias).unwrap(), b"original bytes");
        }
        db.acquire_lock("fail prepared insert").unwrap().execute_batch("CREATE TRIGGER reject_prepared BEFORE INSERT ON metadata_activity_prepared_prerequisite BEGIN SELECT RAISE(ABORT,'synthetic failure'); END;").unwrap();
        assert!(
            db.bind_prepared(&admission, &pending, 0, dir.path(), &stage)
                .await
                .is_err()
        );
        let count: i64 = db
            .acquire_lock("prepared rollback")
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM metadata_activity_prepared_prerequisite",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        db.acquire_lock("allow prepared insert")
            .unwrap()
            .execute_batch("DROP TRIGGER reject_prepared")
            .unwrap();
        db.bind_prepared(&admission, &pending, 0, dir.path(), &stage)
            .await
            .unwrap();
        db.bind_prepared(&admission, &pending, 0, dir.path(), &stage)
            .await
            .unwrap();
        assert!(
            crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
                db.as_ref(),
                &own_pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
            )
            .await
            .is_err()
        );
        let moved = dir.path().join("retained.stage");
        std::fs::rename(&stage, &moved).unwrap();
        std::fs::write(&stage, b"prepared bytes").unwrap();
        assert!(
            db.bind_prepared(&admission, &pending, 0, dir.path(), &stage)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&moved).unwrap(), b"prepared bytes");
        assert_eq!(std::fs::read(&stage).unwrap(), b"prepared bytes");
        assert_eq!(
            active.classify_preflight(&pending, 0, dir.path()).unwrap(),
            ActivityPreflight::Conflicting
        );
        let independent = dir.path().join("independent.jpg");
        std::fs::write(&independent, b"independent").unwrap();
        let mut other = pending.clone();
        other.asset.local_path = Some(independent.clone());
        assert_eq!(
            active.classify_preflight(&other, 0, dir.path()).unwrap(),
            ActivityPreflight::Independent
        );
        #[cfg(unix)]
        {
            let alias = dir.path().join("hardlink.jpg");
            std::fs::hard_link(&media, &alias).unwrap();
            other.asset.local_path = Some(alias);
            assert_eq!(
                active.classify_preflight(&other, 0, dir.path()).unwrap(),
                ActivityPreflight::Conflicting
            );
        }
        let other_root = tempfile::tempdir().unwrap();
        let other_root_file = other_root.path().join("independent.jpg");
        std::fs::write(&other_root_file, b"independent root").unwrap();
        other.asset.local_path = Some(other_root_file);
        assert_eq!(
            active
                .classify_preflight(&other, 0, other_root.path())
                .unwrap(),
            ActivityPreflight::Independent
        );
        // Sidecar-only work is independent while its target is absent;
        // a sidecar hard link to the owned media is a real conflict.
        assert_eq!(
            active
                .classify_preflight(&pending, SIDECAR_COMPONENT_FLAG, dir.path())
                .unwrap(),
            ActivityPreflight::Independent
        );
        #[cfg(unix)]
        {
            let mut name = media.as_os_str().to_os_string();
            name.push(".xmp");
            std::fs::hard_link(&media, std::path::PathBuf::from(name)).unwrap();
            assert_eq!(
                active
                    .classify_preflight(&pending, SIDECAR_COMPONENT_FLAG, dir.path())
                    .unwrap(),
                ActivityPreflight::Conflicting
            );
        }
        let missing = dir.path().join("missing-parent").join("missing.jpg");
        other.asset.local_path = Some(missing);
        assert!(active.classify_preflight(&other, 0, dir.path()).is_err());
        let mut malformed_shape = active.clone();
        let mut malformed_value: serde_json::Value =
            serde_json::from_slice(&malformed_shape.binding).unwrap();
        malformed_value["source_checksum"] = serde_json::json!("not-a-sha256");
        malformed_shape.binding = serde_json::to_vec(&malformed_value).unwrap();
        assert!(
            malformed_shape
                .classify_preflight(&other, 0, dir.path())
                .is_err()
        );
        let mut malformed = active.clone();
        malformed.binding.push(0);
        assert!(
            malformed
                .classify_preflight(&pending, 0, dir.path())
                .is_err()
        );
        // A competing genuine admission remains a conflict even when the
        // first operation presents its own capability; neither is retired.
        let second = db
            .admit_activity(&pending, MetadataRewriteQueue::Ordinary, 0)
            .await
            .unwrap();
        assert!(
            db.validate_conflicts(
                &pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
                Some(&admission)
            )
            .await
            .is_err()
        );
        assert!(
            crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
                db.as_ref(),
                &own_pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
            )
            .await
            .is_err()
        );
        drop(second);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let db2 = Arc::clone(&db);
        let admission2 = Arc::clone(&admission);
        let pending2 = pending.clone();
        let task = tokio::spawn(async move {
            db2.inspect_activity(
                &admission2,
                &pending2,
                MetadataRewriteQueue::Ordinary,
                0,
                Box::new(move || {
                    let _ = entered_tx.send(());
                    release_rx.recv().map_err(|_source| invalid())?;
                    let _ = done_tx.send(());
                    Ok(())
                }),
            )
            .await
        });
        entered_rx.await.unwrap();
        task.abort();
        release_tx.send(()).unwrap();
        done_rx.await.unwrap();
        drop(db);
        let reopened = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
        let restored = reopened.list_activity("PrimarySync").await.unwrap();
        assert_eq!(restored.len(), 2);
        assert!(restored.contains(&active));
        assert!(
            crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
                &reopened,
                &own_pending,
                MetadataRewriteQueue::Ordinary,
                0,
                dir.path(),
            )
            .await
            .is_err()
        );
        // A listed clone is not an admission capability; with no retained
        // admission, restart stays held even when prepared evidence exists.
        assert!(
            reopened
                .validate_conflicts(
                    &pending,
                    MetadataRewriteQueue::Ordinary,
                    0,
                    dir.path(),
                    None
                )
                .await
                .is_err()
        );
        assert!(
            reopened
                .validate_conflicts(
                    &pending,
                    MetadataRewriteQueue::Ordinary,
                    0,
                    dir.path(),
                    Some(&admission)
                )
                .await
                .is_err()
        );
        assert!(
            reopened
                .inspect_activity(
                    &admission,
                    &pending,
                    MetadataRewriteQueue::Ordinary,
                    0,
                    Box::new(|| Ok(())),
                )
                .await
                .is_err()
        );
        let mut altered = active.clone();
        altered.binding.push(0);
        // Unit-test-only malformed capability; production callers cannot mint one.
        let altered_admission = ActivityAdmission {
            activity: altered,
            origin: std::sync::Arc::downgrade(&reopened.conn),
            held: None,
        };
        assert!(
            reopened
                .inspect_activity(
                    &altered_admission,
                    &pending,
                    MetadataRewriteQueue::Ordinary,
                    0,
                    Box::new(|| Ok(()))
                )
                .await
                .is_err()
        );
        let count: i64 = reopened
            .acquire_lock("activity remains")
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM metadata_activity_prerequisite",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        let prepared_count: i64 = reopened
            .acquire_lock("prepared survives reopen")
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM metadata_activity_prepared_prerequisite",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(prepared_count, 1);
        // A fresh synthetic admission on the reopened owner exercises live
        // pending-work cancellation independently of lost continuation.
        let reopened_admission = reopened
            .admit_activity(&pending, MetadataRewriteQueue::Ordinary, 0)
            .await
            .unwrap();
        reopened
            .inspect_activity(
                &reopened_admission,
                &pending,
                MetadataRewriteQueue::Ordinary,
                0,
                Box::new(|| Ok(())),
            )
            .await
            .unwrap();
        reopened.acquire_lock("cancel synthetic pending work").unwrap().execute_batch("UPDATE assets SET metadata_write_failed_at=NULL; UPDATE asset_metadata_paths SET metadata_write_failed_at=NULL;").unwrap();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called2 = Arc::clone(&called);
        assert!(
            reopened
                .inspect_activity(
                    &reopened_admission,
                    &pending,
                    MetadataRewriteQueue::Ordinary,
                    0,
                    Box::new(move || {
                        called2.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })
                )
                .await
                .is_err()
        );
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn path_index_rejects_inconsistent_binding_and_preserves_multilink() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("media");
        let external = dir.path().join("external");
        std::fs::write(&path, b"preserve bytes").unwrap();
        let held = super::HeldFile::open(dir.path(), &path).unwrap();
        let target = held.lookup_target().unwrap();
        let mut asset = TestAssetRecord::new("asset").checksum("source").build();
        asset.local_path = Some(path.clone());
        let pending = PendingMetadataRewrite {
            asset,
            selection_receipt: None,
            activity_admission: None,
            capture_repair_receipt: None,
            source_checksum: None,
        };
        let binding = super::binding(&pending, MetadataRewriteQueue::Ordinary, 0).unwrap();
        let valid = MetadataActivity {
            operation_id: uuid::Uuid::new_v4().to_string(),
            account_key: "a".into(),
            provider_key: "p".into(),
            library: pending.asset.library.to_string(),
            binding: binding.clone(),
        };
        valid.validated_binding().unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(super::DDL).unwrap();
        conn.execute_batch(super::IDENTITY_INDEX_MIGRATION).unwrap();
        conn.execute("INSERT INTO metadata_activity_prerequisite(operation_id,account_key,provider_key,library,binding,target_key,lookup_state) VALUES('one','a','p','PrimarySync',?1,?2,1)", rusqlite::params![binding,target]).unwrap();
        assert!(super::indexed_conflict(&conn, "a", "p", "PrimarySync", &target).unwrap());
        assert!(!super::indexed_conflict(&conn, "a", "p", "PrimarySync", "other").unwrap());
        assert!(conn.execute("INSERT INTO metadata_activity_prerequisite(operation_id,account_key,provider_key,library,binding,target_key,lookup_state) VALUES('bad','a','p','PrimarySync',?1,?2,1)", rusqlite::params![b"{}".as_slice(),target]).is_err());
        let mut malformed: serde_json::Value = serde_json::from_slice(&binding).unwrap();
        malformed["version"] = serde_json::json!(3);
        let malformed = serde_json::to_vec(&malformed).unwrap();
        assert!(conn.execute("INSERT INTO metadata_activity_prerequisite(operation_id,account_key,provider_key,library,binding,target_key,lookup_state) VALUES('bad-version','a','p','PrimarySync',?1,?2,1)", rusqlite::params![malformed,target]).is_err());
        std::fs::hard_link(&path, &external).unwrap();
        let before = std::fs::metadata(&external).unwrap();
        assert!(held.lookup_target().is_err());
        let after = std::fs::metadata(&external).unwrap();
        assert_eq!(
            (before.ino(), before.mtime(), before.mtime_nsec()),
            (after.ino(), after.mtime(), after.mtime_nsec())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve bytes");
        assert_eq!(std::fs::read(&external).unwrap(), b"preserve bytes");
    }

    #[cfg(unix)]
    #[test]
    fn live_competitor_preserves_original_identity_after_name_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let moved = dir.path().join("moved");
        let independent = dir.path().join("independent");
        std::fs::write(&original, b"original bytes").unwrap();
        std::fs::write(&independent, b"independent bytes").unwrap();
        let mut a = TestAssetRecord::new("a").checksum("provider-a").build();
        a.local_path = Some(original.clone());
        let pending_a = PendingMetadataRewrite {
            asset: a,
            selection_receipt: None,
            activity_admission: None,
            capture_repair_receipt: None,
            source_checksum: None,
        };
        let mut b = TestAssetRecord::new("b").checksum("provider-b").build();
        b.local_path = Some(independent.clone());
        let mut pending_b = PendingMetadataRewrite {
            asset: b,
            selection_receipt: None,
            activity_admission: None,
            capture_repair_receipt: None,
            source_checksum: None,
        };
        let owner = owner();
        let (account, provider) = owner.authenticated_keys().unwrap();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(super::DDL).unwrap();
        conn.execute_batch(super::IDENTITY_INDEX_MIGRATION).unwrap();
        let active = MetadataActivity {
            operation_id: uuid::Uuid::new_v4().to_string(),
            account_key: account.into(),
            provider_key: provider.into(),
            library: pending_a.asset.library.to_string(),
            binding: super::binding(&pending_a, MetadataRewriteQueue::Ordinary, 0).unwrap(),
        };
        let held = Arc::new(super::HeldFile::open(dir.path(), &original).unwrap());
        let target = held.lookup_target().unwrap();
        conn.execute("INSERT INTO metadata_activity_prerequisite(operation_id,account_key,provider_key,library,binding,target_key,lookup_state) VALUES(?1,?2,?3,?4,?5,?6,1)", rusqlite::params![active.operation_id, active.account_key, active.provider_key, active.library, active.binding,target]).unwrap();
        let owner_connection = Arc::new(std::sync::Mutex::new(
            rusqlite::Connection::open_in_memory().unwrap(),
        ));
        let origin = Arc::downgrade(&owner_connection);
        super::register_live(&active, &origin, &held).unwrap();
        let capability = ActivityAdmission {
            activity: active,
            origin: origin.clone(),
            held: Some(Arc::clone(&held)),
        };
        let independent_file = super::HeldFile::open(dir.path(), &independent).unwrap();
        super::check_live_competitors(&conn, &owner, &pending_b, &independent_file, &origin, None)
            .unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::write(&original, b"original bytes").unwrap();
        pending_b.asset.local_path = Some(moved.clone());
        let moved_file = super::HeldFile::open(dir.path(), &moved).unwrap();
        assert!(
            super::check_live_competitors(&conn, &owner, &pending_b, &moved_file, &origin, None)
                .is_err()
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"original bytes");
        assert_eq!(std::fs::read(&moved).unwrap(), b"original bytes");
        drop(capability);
        pending_b.asset.local_path = Some(independent.clone());
        assert!(
            super::check_live_competitors(
                &conn,
                &owner,
                &pending_b,
                &independent_file,
                &origin,
                None
            )
            .is_err(),
            "lost proof is unknown, not an independent identity"
        );
    }
}
