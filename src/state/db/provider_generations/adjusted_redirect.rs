//! Fenced in-place repair of incomplete adjusted destinations.
use super::{
    ActiveDecision, SqliteStateDb, account, charge, digest, encode, intent_hash, invalid,
    load_decision, load_generation, pass, refresh_admission_hash, refresh_progress_hash, seal_hash,
    selector_binding,
};
use crate::state::{VersionSizeKey, db::provider_selection::SelectionPath, error::StateError};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

/// A complete snapshot of every affected generation. File validation must hold
/// a confined handle for the original and confirm the sibling is still absent.
pub(crate) struct AdjustedRedirect {
    pub(crate) expected: Vec<(String, ActiveDecision)>,
    pub(crate) fresh_confirmation: Vec<u8>,
    pub(crate) fresh_witnesses: Vec<(String, serde_json::Value)>,
    pub(crate) preferred_pass: String,
    pub(crate) reference_generation: String,
    pub(crate) original_provider_version: VersionSizeKey,
    pub(crate) original: crate::state::db::CrossRenditionOccupant,
    pub(crate) destination: SelectionPath,
}

fn confirmation_for_pass(
    body: &[u8],
    key: &str,
    witnesses: &[(String, serde_json::Value)],
) -> Result<Vec<u8>, StateError> {
    let Some((_, witness)) = witnesses.iter().find(|(pass, _)| pass == key) else {
        return Ok(body.to_vec());
    };
    let mut current: serde_json::Value =
        serde_json::from_slice(body).map_err(|_error| StateError::ProviderWorkConflict)?;
    current
        .as_object_mut()
        .ok_or(StateError::ProviderWorkConflict)?
        .insert("selector_membership".into(), witness.clone());
    serde_json::to_vec(&current).map_err(|_error| StateError::ProviderWorkConflict)
}

impl SqliteStateDb {
    /// Snapshot every exact pending owner. The writer repeats the complete
    /// census inside its transaction and refuses changes or ambiguous aliases.
    pub(crate) async fn adjusted_redirect_snapshots(
        &self,
        owner: account::AccountOwner,
        path: SelectionPath,
    ) -> Result<Vec<(super::ActiveGeneration, ActiveDecision)>, StateError> {
        self.with_conn("read adjusted redirect snapshots", move |conn| {
            account::validate_authenticated(conn, &owner)?;
            let tx = conn.unchecked_transaction()?;
            let rows: Vec<(String,String,String)> = tx.prepare("SELECT generation,pass_key,child FROM provider_active_destinations WHERE version_size='adjusted' AND path=?1 ORDER BY generation,pass_key,child")?
                .query_map([path.key()?], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
            let mut snapshots = Vec::with_capacity(rows.len());
            for (id,key,child) in rows {
                let root = load_generation(&tx,&id)?;
                let decision = load_decision(&tx,&root,&key,&child)?;
                snapshots.push((root,decision));
            }
            tx.commit()?;
            Ok(snapshots)
        }).await
    }
    /// Rebind only exact, incomplete adjusted obligations. SQL validates the
    /// retained receipt, but cannot prove filesystem bytes or namespace: the
    /// mandatory callback must revalidate both while this transaction is open.
    pub(crate) async fn redirect_incomplete_adjusted(
        &self,
        owner: account::AccountOwner,
        mut request: AdjustedRedirect,
        capacity: u64,
        validate_files: Box<dyn FnOnce() -> Result<(), StateError> + Send>,
    ) -> Result<super::ActiveGeneration, StateError> {
        self.with_conn_mut("redirect incomplete adjusted destinations", move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            request.expected.sort_by(|a,b| (&a.0,&a.1.decision.pass_key,&a.1.decision.child).cmp(&(&b.0,&b.1.decision.pass_key,&b.1.decision.child)));
            if request.expected.is_empty()
                || request.fresh_confirmation.len() > super::MAX_CHUNK_BYTES
                || !matches!(request.original_provider_version, VersionSizeKey::Original | VersionSizeKey::Alternative)
                || request.expected.windows(2).any(|pair| matches!(pair, [left, right] if left.0 == right.0 && left.1.decision.pass_key == right.1.decision.pass_key && left.1.decision.child == right.1.decision.child)) {
                return invalid();
            }
            let mut witness_keys = std::collections::HashSet::new();
            if request.fresh_witnesses.iter().any(|(key,_)| !witness_keys.insert(key)) { return invalid(); }
            if !request.expected.iter().any(|(id,_)| id == &request.reference_generation) { return Err(StateError::ProviderWorkConflict); }
            let reference = load_generation(&tx, &request.reference_generation)?;
            let receipt = request.original.receipt;
            let saved = super::super::download_receipts::load(&tx, &receipt.id()?)?
                .ok_or(StateError::ProviderWorkConflict)?;
            if saved != receipt || receipt.version_size != "original" {
                return Err(StateError::ProviderWorkConflict);
            }
            let old_path = receipt.native_path.to_path();
            let new_path = request.destination.to_path();
            let old_key = crate::fs_util::confined_path_key(&old_path).map_err(|_error| StateError::ProviderSelectionInvalid)?;
            let new_key = crate::fs_util::confined_path_key(&new_path).map_err(|_error| StateError::ProviderSelectionInvalid)?;
            if old_key == new_key || old_path.parent() != new_path.parent()
                || SelectionPath::from_path(&new_path) != request.destination
                || !new_path.is_absolute() {
                return Err(StateError::ProviderWorkConflict);
            }
            let new_text = new_path.to_str().ok_or(StateError::ProviderWorkConflict)?;
            match super::super::primary_layout::guard_writer(&tx, &new_path) {
                Ok(()) => {},
                Err(StateError::Invariant { operation: "primary_layout", .. }) => return Err(StateError::ProviderWorkConflict),
                Err(error) => return Err(error),
            }
            let journal: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM owned_temp_files WHERE publication IS NOT NULL AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')=?2)",
                params![receipt.library, receipt.state_id], |r| r.get(0),
            )?;
            if journal { return Err(StateError::ProviderWorkConflict); }
            let selected_path = receipt.native_path.key()?;
            // A second receipt at the same normalized native path makes the
            // original occupant ambiguous, even if this receipt is valid.
            let receipt_id = receipt.id()?;
            let mut receipt_paths = tx.prepare("SELECT id,native_path FROM download_receipts")?;
            for row in receipt_paths.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
                let (id,path) = row?;
                let path: SelectionPath = serde_json::from_str(&path).map_err(|_error| StateError::ProviderWorkConflict)?;
                let key = crate::fs_util::confined_path_key(&path.to_path()).map_err(|_error| StateError::ProviderWorkConflict)?;
                if key == old_key && id != receipt_id { return Err(StateError::ProviderWorkConflict); }
                if key == new_key { return Err(StateError::ProviderWorkConflict); }
            }
            drop(receipt_paths);
            let mut actual: Vec<(String,String,String)> = tx.prepare(
                "SELECT generation,pass_key,child FROM provider_active_destinations WHERE version_size='adjusted' AND path=?1 ORDER BY generation,pass_key,child"
            )?.query_map([&selected_path], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
            actual.sort();
            let wanted: Vec<_> = request.expected.iter().map(|(id,d)| (id.clone(),d.decision.pass_key.clone(),d.decision.child.clone())).collect();
            if actual != wanted { return Err(StateError::ProviderWorkConflict); }
            // Check all active path spellings, including completed rows that
            // are absent from the pending-path cache.
            let mut active = tx.prepare("SELECT g.scope,w.generation,w.pass_key,w.child,w.master,w.library,w.asset_id,w.version_size,w.path,w.verified_media,w.source_checksum,w.local_checksum FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation")?;
            for row in active.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,String>(8)?,r.get::<_,bool>(9)?,r.get::<_,Option<String>>(10)?,r.get::<_,Option<String>>(11)?)))? {
                let (scope,id,pass,child,master,library,asset,version,path,verified,source,local)=row?;
                let path: SelectionPath=serde_json::from_str(&path).map_err(|_error|StateError::ProviderWorkConflict)?;
                let key=crate::fs_util::confined_path_key(&path.to_path()).map_err(|_error|StateError::ProviderWorkConflict)?;
                if key == new_key { return Err(StateError::ProviderWorkConflict); }
                if key == old_key {
                    let original = scope == receipt.scope && child == receipt.child && master.as_deref() == Some(receipt.master.as_str()) && library == receipt.library && asset == receipt.state_id && version == "original" && verified && source.as_deref() == Some(receipt.source_checksum.as_str()) && local.as_deref() == Some(receipt.local_checksum.as_str()) && receipt.fulfills_pass(&pass);
                    let adjusted = version == "adjusted" && wanted.contains(&(id,pass,child));
                    if !original && !adjusted { return Err(StateError::ProviderWorkConflict); }
                }
            }
            drop(active);
            let original_chosen = super::super::primary_layout::path_key(&old_path)?;
            let candidates = super::pending_path_cache::candidates(&tx, &original_chosen)?;
            if candidates != wanted { return Err(StateError::ProviderWorkConflict); }
            let chosen = super::super::primary_layout::path_key(&new_path)?;
            if !super::pending_path_cache::candidates(&tx, &chosen)?.is_empty() {
                return Err(StateError::ProviderWorkConflict);
            }
            // Existing catalogue and durable reservations are an independent
            // ownership census. Unknown paths are a refusal, not absence proof.
            for table in ["assets", "asset_metadata_paths"] {
                let sql = format!("SELECT library,id,version_size,local_path FROM {table} WHERE local_path IS NOT NULL");
                let mut statement = tx.prepare(&sql)?;
                let rows = statement.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?;
                for row in rows {
                    let (library,id,version,path) = row?;
                    let key = crate::fs_util::confined_path_key(std::path::Path::new(&path)).map_err(|_error| StateError::ProviderWorkConflict)?;
                    if key == new_key || (key == old_key && (library != receipt.library || id != receipt.state_id || (version != "original" && version != "adjusted"))) {
                        return Err(StateError::ProviderWorkConflict);
                    }
                }
            }
            let conflict: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM reconciliation_paths WHERE destination_path_key=?1)", [&new_key], |r| r.get(0))?;
            if conflict { return Err(StateError::ProviderWorkConflict); }
            let mut updated = Vec::new();
            let mut accounted = std::collections::HashSet::new();
            let mut historical_content = std::collections::HashSet::new();
            for (id, expected) in &request.expected {
                let root = load_generation(&tx, id)?;
                if root.spec.format != 2 || root.spec.scope != receipt.scope || !root.sealed
                    || expected.state_id != receipt.state_id || expected.decision.child != receipt.child
                    || expected.decision.master.as_deref() != Some(receipt.master.as_str()) {
                    return Err(StateError::ProviderWorkConflict);
                }
                let old = load_decision(&tx, &root, &expected.decision.pass_key, &expected.decision.child)?;
                if old != *expected { return Err(StateError::ProviderWorkConflict); }
                let seal: String = tx.query_row("SELECT seal_hash FROM provider_active_generations WHERE id=?1", [id], |r|r.get(0))?;
                if seal_hash(&tx, &root, root.checkpoint_ready, root.checkpoint_veto.as_deref())? != seal { return invalid(); }
                let master = old.decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?;
                let previous = crate::icloud::photos::current_asset(old.decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?, &old.decision.child, master, &root.spec.zone).map_err(|_error| StateError::ProviderSelectionInvalid)?;
                let current = crate::icloud::photos::current_asset(&request.fresh_confirmation, &old.decision.child, master, &root.spec.zone).map_err(|_error| StateError::ProviderWorkConflict)?;
                let original = current.versions().iter().find_map(|(key,value)| (VersionSizeKey::from(*key) == request.original_provider_version).then(|| (value.checksum.as_ref(),value.size)));
                if original != Some((receipt.provider_checksum.as_str(),receipt.provider_size)) { return Err(StateError::ProviderWorkConflict); }
                let mut next = old.clone();
                let selected: Vec<_> = next.decision.destinations.iter_mut().filter(|d| d.version_size == "adjusted" && d.path == receipt.native_path).collect();
                if selected.len() != 1 { return Err(StateError::ProviderWorkConflict); }
                let d = selected.into_iter().next().ok_or(StateError::ProviderSelectionInvalid)?;
                let selected_checksum = d.checksum.clone();
                let selected_size = d.size;
                let target_pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size='adjusted' AND path=?4 AND admitted=1 AND verified_media=0)",params![id,old.decision.pass_key,old.decision.child,selected_path],|r|r.get(0))?;
                if !target_pending { return Err(StateError::ProviderWorkConflict); }
                accounted.insert((selected_checksum.clone(), i64::try_from(selected_size).map_err(|_error|StateError::ProviderSelectionInvalid)?));
                // A different saved provider generation remains its own debt.
                // Its authenticated manifest and seal are validated above, but
                // no current bytes or completion are inferred for it.
                let compatible = root.spec.zone == reference.spec.zone
                    && root.spec.metadata_flags == reference.spec.metadata_flags
                    && root.spec.metadata_enabled == reference.spec.metadata_enabled
                    && (root.spec.profile == reference.spec.profile
                        || super::retained_profiles_compatible(&root.spec.profile, &reference.spec.profile));
                if !compatible || !crate::icloud::photos::same_selected_facts(&previous, &current) {
                    historical_content.insert((selected_checksum, selected_size));
                    continue;
                }
                if !receipt.fulfills_pass(&old.decision.pass_key) { return Err(StateError::ProviderWorkConflict); }
                let fresh = confirmation_for_pass(&request.fresh_confirmation, &old.decision.pass_key, &request.fresh_witnesses)?;
                if fresh.len() > super::MAX_CHUNK_BYTES { return invalid(); }
                selector_binding::validate_membership(&root.spec, pass(&root, &old.decision.pass_key)?, &old.decision.child, &fresh)
                    .map_err(|_error| StateError::ProviderWorkConflict)?;
                let adjusted = current.versions().iter().find_map(|(key,value)| (VersionSizeKey::from(*key) == VersionSizeKey::Adjusted).then(|| (value.checksum.as_ref(),value.size)));
                if adjusted != Some((selected_checksum.as_str(),selected_size)) { return Err(StateError::ProviderWorkConflict); }
                d.path = request.destination.clone();
                // Completed siblings are retained; their existing proofs were
                // validated by load_decision and are rehashed below. Prepared
                // or frozen metadata binds the old manifest and cannot move.
                let blocked: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND (prepared_hash IS NOT NULL OR frozen_metadata IS NOT NULL OR admitted=0 OR (version_size='adjusted' AND path=?4 AND verified_media=1)))", params![id,old.decision.pass_key,old.decision.child,selected_path], |r|r.get(0))?;
                if blocked { return Err(StateError::ProviderWorkConflict); }
                updated.push((root,old,next,selected_checksum,selected_size));
            }
            // Preserve backoff but choose a currently due matching owner when
            // multiple equivalent generations can dispatch this resource.
            let mut dispatch: Option<(bool,i64,String)> = None;
            let now=chrono::Utc::now().timestamp();
            for (root,old,_,_,_) in &updated {
                if old.decision.pass_key != request.preferred_pass { continue; }
                let deadline:i64=tx.query_row("SELECT next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,old.decision.pass_key,old.decision.child],|r|r.get(0))?;
                let candidate=(deadline > now,deadline,root.id.clone());
                if dispatch.as_ref().is_none_or(|previous| candidate < *previous) { dispatch=Some(candidate); }
            }
            let selected_id = dispatch.ok_or(StateError::ProviderWorkConflict)?.2;
            // This legacy reservation key omits metadata. A saved row shared
            // by different decisions with identical bytes cannot be moved
            // without also changing the older decision's ownership evidence.
            for (_,_,_,checksum,size) in &updated {
                if historical_content.contains(&(checksum.clone(),*size)) {
                    let saved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM reconciliation_paths WHERE library=?1 AND id=?2 AND version_size='adjusted' AND requested_path_key=?3 AND destination_path_key=?3 AND provider_checksum=?4 AND provider_size=?5)",params![receipt.library,receipt.state_id,old_key,checksum,i64::try_from(*size).map_err(|_error|StateError::ProviderSelectionInvalid)?],|r|r.get(0))?;
                    if saved { return Err(StateError::ProviderWorkConflict); }
                }
            }
            // Reservations at the original may belong to the validated older
            // debts; retain those exact tuples and reject every other owner.
            {
                let mut statement=tx.prepare("SELECT library,id,version_size,provider_checksum,provider_size FROM reconciliation_paths WHERE destination_path_key=?1")?;
                for row in statement.query_map([&old_key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?)))? {
                    let (library,id,version,checksum,size)=row?;
                    let same = library==receipt.library && id==receipt.state_id;
                    let original = version=="original" && checksum==receipt.provider_checksum && u64::try_from(size).ok()==Some(receipt.provider_size);
                    let adjusted = version=="adjusted" && accounted.contains(&(checksum,size));
                    if !same || !(original || adjusted) { return Err(StateError::ProviderWorkConflict); }
                }
            }
            for (root,old,next,checksum,size) in &updated {
                let old_charge: i64 = tx.query_row("SELECT charged_bytes FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3", params![root.id,old.decision.pass_key,old.decision.child],|r|r.get(0))?;
                let extra = u64::try_from(old_charge).map_err(|_error| StateError::ProviderSelectionInvalid)?.checked_sub(super::decision_charge(root,old)?).ok_or(StateError::ProviderSelectionInvalid)?;
                let new_charge = super::decision_charge(root,next)?.checked_add(extra).ok_or(StateError::ProviderSelectionFull)?;
                let changed = tx.execute("UPDATE provider_active_destinations SET path=?6,compat_path=?7 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size='adjusted' AND path=?4 AND checksum=?5 AND admitted=1 AND verified_media=0", params![root.id,old.decision.pass_key,old.decision.child,selected_path,checksum,request.destination.key()?,new_path.to_string_lossy()])?;
                if changed != 1 { return Err(StateError::ProviderWorkConflict); }
                let bytes=encode(next)?;
                tx.execute("UPDATE provider_active_decisions SET manifest=?4,manifest_hash=?5,charged_bytes=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3", params![root.id,old.decision.pass_key,old.decision.child,bytes,digest(&bytes),i64::try_from(new_charge).map_err(|_error| StateError::ProviderSelectionFull)?])?;
                refresh_admission_hash(&tx,root,&old.decision.pass_key,&old.decision.child)?;
                for destination in &next.decision.destinations {
                    let group:Option<String> = tx.query_row("SELECT grouping_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,old.decision.pass_key,old.decision.child,destination.version_size,destination.path.key()?],|r|r.get(0))?;
                    if let Some(group) = group { let intent = intent_hash(root,next,destination,&group)?; tx.execute("UPDATE provider_active_destinations SET intent_hash=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,old.decision.pass_key,old.decision.child,destination.version_size,destination.path.key()?,intent])?; }
                    refresh_progress_hash(&tx,root,next,destination)?;
                }
                super::reference_refresh::reseal(&tx,root)?;
                let size = i64::try_from(*size).map_err(|_error| StateError::ProviderSelectionInvalid)?;
                tx.execute("UPDATE reconciliation_paths SET destination_path_key=?6,destination_path=?7 WHERE library=?1 AND id=?2 AND version_size='adjusted' AND requested_path_key=?3 AND destination_path_key=?3 AND provider_checksum=?4 AND provider_size=?5",params![receipt.library,receipt.state_id,old_key,checksum,size,new_key,new_text])?;
                let saved: Option<String> = tx.query_row("SELECT destination_path_key FROM reconciliation_paths WHERE library=?1 AND id=?2 AND version_size='adjusted' AND requested_path_key=?3 AND provider_checksum=?4 AND provider_size=?5",params![receipt.library,receipt.state_id,old_key,checksum,size],|r|r.get(0)).optional()?;
                match saved {
                    Some(path) if path != new_key => return Err(StateError::ProviderWorkConflict),
                    Some(_) => {},
                    None => { tx.execute("INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES(?1,?2,'adjusted',?3,?4,?5,?6,?7)",params![receipt.library,receipt.state_id,old_key,new_key,new_text,checksum,size])?; }
                }
            }
            charge(&tx,0,capacity,2)?;
            // Recheck held file identities and bytes at the last fallible boundary.
            validate_files()?;
            let selected_root = load_generation(&tx, &selected_id)?;
            tx.commit()?;
            Ok(selected_root)
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::confirmation_for_pass;
    use serde_json::{Value, json};

    #[test]
    fn each_pass_uses_its_own_confirmed_membership_witness() {
        let body =
            serde_json::to_vec(&json!({"records":[],"selector_membership":{"container":"old"}}))
                .unwrap();
        let witnesses = vec![
            ("a".to_owned(), json!({"container":"one"})),
            ("b".to_owned(), json!({"container":"two"})),
        ];
        for (key, container) in [("a", "one"), ("b", "two")] {
            let current: Value =
                serde_json::from_slice(&confirmation_for_pass(&body, key, &witnesses).unwrap())
                    .unwrap();
            assert_eq!(current["selector_membership"]["container"], container);
            assert_eq!(current["records"], json!([]));
        }
    }
}
