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
}

impl std::fmt::Debug for ActivityAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityAdmission").finish_non_exhaustive()
    }
}

impl ActivityAdmission {
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
#[cfg(test)]
struct HeldFile {
    path: crate::fs_util::ConfinedPath,
    file: std::fs::File,
    identity: crate::fs_util::FileIdentity,
    size: u64,
    sha256: String,
}

#[cfg(test)]
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
    fn evidence(&self) -> serde_json::Value {
        serde_json::json!({"path":super::provider_selection::SelectionPath::from_path(self.path.path()),"identity":self.identity,"size":self.size,"sha256":self.sha256})
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
    /// Existing active records are never evidence of completed content. This
    /// read-only preflight cannot authorize a filesystem mutation.
    fn classify_preflight(
        &self,
        pending: &PendingMetadataRewrite,
        flags: u8,
        root: &std::path::Path,
    ) -> Result<ActivityPreflight, StateError> {
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
                "INSERT INTO metadata_activity_prerequisite VALUES(?1,?2,?3,?4,?5)",
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
            })
        })
        .await
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
        let continuation = continuation.map(|capability| capability.activity.clone());
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
                if continuation.as_ref() == Some(&active) {
                    if binding(&pending,queue,flags)? != active.binding { return Err(invalid()); }
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
        crate::state::db::MetadataRewriteStore::validate_metadata_activity_conflicts(
            db.as_ref(),
            &own_pending,
            MetadataRewriteQueue::Ordinary,
            0,
            dir.path(),
        )
        .await
        .unwrap();
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
        db.validate_conflicts(
            &pending,
            MetadataRewriteQueue::Ordinary,
            0,
            dir.path(),
            Some(&admission),
        )
        .await
        .unwrap();
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
}
