//! Atomic handoff of unpublished destinations after a current child changes master.
use super::{
    ActiveDecision, ActiveGeneration, MAX_CHUNK_BYTES, SqliteStateDb, account, charge, digest,
    encode, freeze_metadata_identity, grouping_hash, intent_hash, invalid, load_decision,
    load_generation, owner_keys, refresh_admission_hash, refresh_progress_hash, retirement,
    seal_hash, seal_header, selector_binding, validate_decision, validate_records,
};
use crate::state::{AssetRecord, error::StateError};
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior, params};

fn resource_facts(
    asset: &crate::icloud::photos::PhotoAsset,
    version: crate::state::VersionSizeKey,
) -> Option<(String, u64)> {
    asset.versions().iter().find_map(|(key, value)| {
        (crate::state::VersionSizeKey::from(*key) == version)
            .then(|| (value.checksum.to_string(), value.size))
    })
}

fn reseal(conn: &Connection, root: &ActiveGeneration) -> Result<(), StateError> {
    if root.sealed {
        let hash = seal_hash(
            conn,
            root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
        )?;
        let header = seal_header(
            root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
            &hash,
        )?;
        conn.execute(
            "UPDATE provider_active_generations SET seal_hash=?2,seal_header_hash=?3 WHERE id=?1",
            params![root.id, hash, header],
        )?;
    }
    Ok(())
}

impl SqliteStateDb {
    /// The caller has freshly confirmed the same authenticated child/source and
    /// selecting membership. SQLite independently binds that proof to the exact
    /// predecessor and immutable admitted paths. No completed receipt is updated.
    pub(crate) async fn refresh_selection_reference(
        &self,
        owner: account::AccountOwner,
        generation: String,
        expected: ActiveDecision,
        mut replacement: ActiveDecision,
        selected: (
            Vec<AssetRecord>,
            Vec<(
                String,
                crate::state::VersionSizeKey,
                crate::state::VersionSizeKey,
            )>,
        ),
        capacity: u64,
    ) -> Result<ActiveGeneration, StateError> {
        self.with_conn_mut("refreshing unpublished selected reference", move |conn| {
            let (mut records, selected_versions)=selected;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            account::validate_authenticated(&tx, &owner)?;
            let root = load_generation(&tx, &generation)?;
            selector_binding::validate_owner(&tx, &root.spec)?;
            if root.spec.format != 2 || expected.state_id != expected.decision.child
                || replacement.state_id != expected.state_id
                || replacement.decision.child != expected.decision.child
                || replacement.decision.pass_key != expected.decision.pass_key
                || replacement.decision.master == expected.decision.master
                || replacement.decision.confirmation.as_ref().is_none_or(|b|b.len()>MAX_CHUNK_BYTES)
            { return invalid(); }
            let old = load_decision(&tx, &root, &expected.decision.pass_key, &expected.decision.child)?;
            if old != expected { return Err(StateError::ProviderWorkConflict); }
            if root.sealed {
                let saved:String = tx.query_row("SELECT seal_hash FROM provider_active_generations WHERE id=?1",[&generation],|r|r.get(0))?;
                if seal_hash(&tx,&root,root.checkpoint_ready,root.checkpoint_veto.as_deref())? != saved {return invalid();}
            }
            let library = root.spec.zone.get("zoneName").and_then(serde_json::Value::as_str).ok_or(StateError::ProviderSelectionInvalid)?;
            freeze_metadata_identity(&tx,library,&old.state_id)?;
            let journal:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM owned_temp_files WHERE publication IS NOT NULL AND json_extract(publication,'$.library')=?1 AND json_extract(publication,'$.id')=?2)",params![library,old.state_id],|r|r.get(0))?;
            if journal {return Err(StateError::ProviderWorkConflict);}
            let mut predecessor=old.clone();
            predecessor.decision.destinations.clear();
            let mut pending=Vec::new();
            for destination in &old.decision.destinations {
                let (media,metadata,prepared,frozen):(bool,bool,bool,bool)=tx.query_row("SELECT verified_media,verified_metadata,prepared_hash IS NOT NULL,frozen_metadata IS NOT NULL FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,old.decision.pass_key,old.decision.child,destination.version_size,destination.path.key()?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                if prepared || frozen || (media && root.spec.metadata_enabled && !metadata) {return Err(StateError::ProviderWorkConflict);}
                if media {
                    // Validate existing completed provenance before any hash changes.
                    let mut receipt=retirement::receipt(&tx,&root,&old,destination)?;
                    let saved=super::super::download_receipts::load(&tx,&receipt.id()?)?.ok_or(StateError::ProviderSelectionInvalid)?;
                    if !saved.fulfills_pass(&old.decision.pass_key) {return Err(StateError::ProviderWorkConflict);}
                    receipt.origin_pass=saved.origin_pass.clone();
                    if saved != receipt {return Err(StateError::ProviderWorkConflict);}
                    predecessor.decision.destinations.push(destination.clone());
                } else {
                    let current=replacement.decision.destinations.iter().find(|d|d.version_size==destination.version_size && d.path==destination.path).ok_or(StateError::ProviderWorkConflict)?;
                    pending.push(current.clone());
                }
            }
            if pending.is_empty() {return Err(StateError::ProviderWorkConflict);}
            replacement.decision.destinations=pending;
            records.retain(|record|replacement.decision.destinations.iter().any(|d|d.version_size==record.version_size.as_str()));
            let previous_asset=crate::icloud::photos::current_asset(old.decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&old.decision.child,old.decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&root.spec.zone).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
            let current_asset=crate::icloud::photos::current_asset(replacement.decision.confirmation.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&replacement.decision.child,replacement.decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?,&root.spec.zone).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
            if old.decision.destinations.iter().any(|d|!selected_versions.iter().any(|(version,_,_)|version==&d.version_size)) {return Err(StateError::ProviderWorkConflict);}
            let mut logical_versions=std::collections::HashSet::new();
            for (version, previous_key, current_key) in &selected_versions {
                if !logical_versions.insert(version) {return Err(StateError::ProviderWorkConflict);}
                let previous=resource_facts(&previous_asset,*previous_key).ok_or(StateError::ProviderWorkConflict)?;
                let current=resource_facts(&current_asset,*current_key).ok_or(StateError::ProviderWorkConflict)?;
                if let Some(destination)=replacement.decision.destinations.iter().find(|d|d.version_size==*version) {
                    if (destination.checksum.clone(),destination.size)!=current {return Err(StateError::ProviderWorkConflict);}
                } else if previous!=current {return Err(StateError::ProviderWorkConflict);}
            }
            validate_decision(&tx,&root,&replacement,true)?;
            validate_records(&root,&replacement,&records)?;
            // Refuse legacy projected work and unrelated owners using the existing guards.
            for record in &records {super::super::provider_work::guard_projected_generation(&tx,record)?;}
            super::super::provider_work::guard_projected_mapping(&tx,library,&old.decision.child,replacement.decision.master.as_deref().ok_or(StateError::ProviderSelectionInvalid)?)?;
            for destination in &replacement.decision.destinations {
                let path=destination.path.to_path();
                let chosen=super::super::primary_layout::path_key(&path)?;
                let native=crate::fs_util::confined_path_key(&path).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                for (other_id,key,child) in super::pending_path_cache::candidates(&tx,&chosen)? {
                    let other_root=load_generation(&tx,&other_id)?;
                    let other=load_decision(&tx,&other_root,&key,&child)?;
                    for other_destination in &other.decision.destinations {
                        let other_native=crate::fs_util::confined_path_key(&other_destination.path.to_path()).map_err(|_invalid|StateError::ProviderSelectionInvalid)?;
                        let transferred=other_id==generation && key==old.decision.pass_key && child==old.decision.child && other_destination.version_size==destination.version_size && other_destination.path==destination.path;
                        if other_native==native && !transferred {return Err(StateError::ProviderWorkConflict);}
                    }
                }
            }
            let id=format!("{:032x}",rand::random::<u128>());
            let (account,provider)=owner_keys(&tx)?;
            let spec=encode(&root.spec)?;
            let successor=ActiveGeneration{id:id.clone(),spec:root.spec.clone(),sealed:false,checkpoint_ready:false,checkpoint_veto:None};
            let manifest=encode(&replacement)?;
            let header_charge=u64::try_from(spec.len()+id.len()+account.len()+provider.len()+root.spec.scope.len()+root.spec.config_hash.len()+root.spec.basis.len()+128).map_err(|_invalid|StateError::ProviderSelectionFull)?;
            let work_charge=super::decision_charge(&successor,&replacement)?;
            let old_charge:i64=tx.query_row("SELECT charged_bytes FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,old.decision.pass_key,old.decision.child],|r|r.get(0))?;
            let extra=u64::try_from(old_charge).map_err(|_invalid|StateError::ProviderSelectionInvalid)?.checked_sub(super::decision_charge(&root,&old)?).ok_or(StateError::ProviderSelectionInvalid)?;
            let remaining_charge=super::decision_charge(&root,&predecessor)?.checked_add(extra).ok_or(StateError::ProviderSelectionFull)?;
            tx.execute("INSERT INTO provider_active_generations(id,account_key,provider_key,scope,config_hash,basis,specification,specification_hash,created_at,charged_bytes,metadata_enabled) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![id,account,provider,root.spec.scope,root.spec.config_hash,root.spec.basis,spec,digest(&spec),Utc::now().timestamp_millis(),header_charge as i64,root.spec.metadata_enabled])?;
            let d=&replacement.decision;
            tx.execute("INSERT INTO provider_active_decisions(generation,pass_key,child,manifest,manifest_hash,outcome,admission,reason,charged_bytes) VALUES(?1,?2,?3,?4,?5,'selected','admitted','',?6)",params![id,d.pass_key,d.child,manifest,digest(&manifest),work_charge as i64])?;
            // Copy only the exact reservation being transferred. Existing other
            // reservations retain their own tuples and remain authoritative.
            for destination in &d.destinations {
                let path=destination.path.key()?;
                let record=records.iter().find(|r|r.version_size.as_str()==destination.version_size).ok_or(StateError::ProviderSelectionInvalid)?;
                tx.execute("INSERT INTO provider_active_destinations(generation,pass_key,child,library,asset_id,master,version_size,path,checksum,size_bytes,created_at,added_at,metadata_hash,admitted,compat_path) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,1,?14)",params![id,d.pass_key,d.child,library,replacement.state_id,d.master,destination.version_size,path,destination.checksum,record.size_bytes as i64,super::encode_asset_date(record.created_at),record.added_at.map(super::encode_asset_date),destination.metadata_hash,destination.path.to_path().to_string_lossy()])?;
                tx.execute("DELETE FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND verified_media=0",params![generation,d.pass_key,d.child,destination.version_size,path])?;
            }
            #[cfg(all(test,target_os="linux"))]
            crate::test_helpers::process_death_point("selection-reference-transferred");
            let previous=encode(&predecessor)?;
            tx.execute("UPDATE provider_active_decisions SET manifest=?4,manifest_hash=?5,attempts=0,next_retry_at=0,reason='',charged_bytes=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3",params![generation,d.pass_key,d.child,previous,digest(&previous),i64::try_from(remaining_charge).map_err(|_invalid|StateError::ProviderSelectionFull)?])?;
            refresh_admission_hash(&tx,&root,&d.pass_key,&d.child)?;
            for destination in &predecessor.decision.destinations {
                let group:Option<String>=tx.query_row("SELECT grouping_hash FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,d.pass_key,d.child,destination.version_size,destination.path.key()?],|r|r.get(0))?;
                if let Some(group)=group {let intent=intent_hash(&root,&predecessor,destination,&group)?;tx.execute("UPDATE provider_active_destinations SET intent_hash=?6 WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,d.pass_key,d.child,destination.version_size,destination.path.key()?,intent])?;}
                refresh_progress_hash(&tx,&root,&predecessor,destination)?;
            }
            reseal(&tx,&root)?;
            refresh_admission_hash(&tx,&successor,&d.pass_key,&d.child)?;
            for record in &records {super::upsert_asset_row(&tx,record,Utc::now().timestamp())?;}
            tx.execute("INSERT INTO asset_master_mappings(library,asset_record_name,master_record_name,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(library,asset_record_name) DO UPDATE SET master_record_name=excluded.master_record_name,updated_at=excluded.updated_at",params![library,d.child,d.master,Utc::now().timestamp()])?;
            let group=grouping_hash(&tx,library,&replacement.state_id)?;
            super::freeze_asset_intents(&tx,&successor,library,&replacement.state_id,&group)?;
            let hash=seal_hash(&tx,&successor,false,None)?;
            let header=seal_header(&successor,false,None,&hash)?;
            tx.execute("UPDATE provider_active_generations SET sealed=1,seal_hash=?2,seal_header_hash=?3 WHERE id=?1",params![id,hash,header])?;
            while retirement::compact_batch(&tx,Some(&root.id),2)? > 0 {}
            // Capacity is the committed ownership state, not a temporary sum
            // counting the transferred predecessor and successor twice.
            charge(&tx,0,capacity,2)?;
            let out=load_generation(&tx,&id)?;
            tx.commit()?;
            #[cfg(all(test,target_os="linux"))]
            crate::test_helpers::process_death_point("selection-reference-committed");
            Ok(out)
        }).await
    }
}

#[cfg(test)]
impl SqliteStateDb {
    pub(crate) fn merge_test_completed_origin(
        conn: &Connection,
        generation: &str,
        key: &str,
        child: &str,
        origin: &str,
    ) -> Result<super::super::download_receipts::DownloadReceipt, StateError> {
        let root = load_generation(conn, generation)?;
        let manifest = load_decision(conn, &root, key, child)?;
        let destination=manifest.decision.destinations.iter().find(|destination| {
            conn.query_row::<bool,_,_>("SELECT verified_media FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![generation,key,child,destination.version_size,destination.path.key().unwrap()],|r|r.get(0)).unwrap()
        }).ok_or(StateError::ProviderSelectionInvalid)?;
        let mut receipt = retirement::receipt(conn, &root, &manifest, destination)?;
        receipt.save(conn)?;
        receipt.origin_pass = origin.to_owned();
        let id = receipt.save(conn)?;
        super::super::download_receipts::load(conn, &id)?
            .ok_or(StateError::ProviderSelectionInvalid)
    }
}
