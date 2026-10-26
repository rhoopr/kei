//! Read-only recorded-work reporting. Never a provider census or checkpoint gate.
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, params};

use super::{ActiveGeneration, SqliteStateDb, encode, load_decision, load_generation, seal_hash};
use crate::state::error::StateError;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DestinationState {
    Verified,
    MetadataPending,
    MediaPending,
}

#[derive(Default)]
pub(crate) struct RecordedProfileWork {
    pub(crate) roots: u64,
    pub(crate) unsealed_or_held_roots: u64,
    pub(crate) verified_destinations: u64,
    pub(crate) pending_media: u64,
    pub(crate) pending_metadata: u64,
    pub(crate) deferred_decisions: u64,
    pub(crate) excluded_decisions: u64,
}

#[derive(Default)]
pub(crate) struct RecordedSelectionWork {
    pub(crate) profiles: Vec<RecordedProfileWork>,
    pub(crate) historical_roots: u64,
    pub(crate) unsupported_profiles: u64,
    pub(crate) invalid_roots: u64,
    pub(crate) stored_media_receipts: u64,
    pub(crate) invalid_receipts: u64,
    pub(crate) current_source_observations: u64,
    pub(crate) historical_source_observations: u64,
    pub(crate) unclassified_source_observations: u64,
}

#[derive(Default)]
struct ProfileAccumulator {
    roots: u64,
    unsealed_or_held_roots: u64,
    // Include resource and requested metadata identity. Separate selected passes
    // and destinations stay separate even when their physical bytes coincide.
    destinations: BTreeMap<Vec<u8>, DestinationState>,
    deferred: BTreeSet<Vec<u8>>,
    excluded: BTreeSet<Vec<u8>>,
}

impl ProfileAccumulator {
    fn merge(&mut self, other: Self) {
        self.roots += other.roots;
        self.unsealed_or_held_roots += other.unsealed_or_held_roots;
        self.deferred.extend(other.deferred);
        self.excluded.extend(other.excluded);
        for (key, state) in other.destinations {
            self.destinations
                .entry(key)
                .and_modify(|prior| *prior = (*prior).max(state))
                .or_insert(state);
        }
    }

    fn finish(self) -> RecordedProfileWork {
        let mut result = RecordedProfileWork {
            roots: self.roots,
            unsealed_or_held_roots: self.unsealed_or_held_roots,
            ..Default::default()
        };
        for state in self.destinations.into_values() {
            match state {
                DestinationState::Verified => result.verified_destinations += 1,
                DestinationState::MetadataPending => result.pending_metadata += 1,
                DestinationState::MediaPending => result.pending_media += 1,
            }
        }
        for _ in self.deferred {
            result.deferred_decisions += 1;
        }
        for _ in self.excluded {
            result.excluded_decisions += 1;
        }
        result
    }
}

fn read_root(conn: &Connection, root: &ActiveGeneration) -> Result<ProfileAccumulator, StateError> {
    if root.sealed {
        let saved: String = conn.query_row(
            "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
            [&root.id],
            |row| row.get(0),
        )?;
        if seal_hash(
            conn,
            root,
            root.checkpoint_ready,
            root.checkpoint_veto.as_deref(),
        )? != saved
        {
            return Err(StateError::ProviderSelectionInvalid);
        }
    }
    let mut result = ProfileAccumulator {
        roots: 1,
        unsealed_or_held_roots: u64::from(
            !root.sealed || !root.checkpoint_ready || root.checkpoint_veto.is_some(),
        ),
        ..Default::default()
    };
    let keys:Vec<(String,String)>=conn.prepare("SELECT pass_key,child FROM provider_active_decisions WHERE generation=?1 ORDER BY pass_key,child")?
        .query_map([&root.id],|row|Ok((row.get(0)?,row.get(1)?)))?.collect::<Result<_,_>>()?;
    for (key, child) in keys {
        let manifest = load_decision(conn, root, &key, &child)?;
        let (admission, reason):(String,String)=conn.query_row("SELECT admission,reason FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3",params![root.id,key,child],|row|Ok((row.get(0)?,row.get(1)?)))?;
        let decision_key = encode(&(&key, &child, &manifest.decision.master, &reason))?;
        if admission == "excluded" {
            result.excluded.insert(decision_key);
        } else if admission == "admitted" && manifest.decision.destinations.is_empty()
            && conn.query_row("SELECT compacted_hash IS NOT NULL FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3", params![root.id,key,child], |row| row.get::<_, bool>(0))?
        {
            // load_decision already validated the compact proof. A successor
            // can own every unfinished destination: this predecessor owes none.
        } else if admission != "admitted" || manifest.decision.destinations.is_empty() {
            result.deferred.insert(decision_key);
        } else {
            for destination in &manifest.decision.destinations {
                let (media,metadata):(bool,bool)=conn.query_row("SELECT verified_media,verified_metadata FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5",params![root.id,key,child,destination.version_size,destination.path.key()?],|row|Ok((row.get(0)?,row.get(1)?)))?;
                let state = if !media {
                    DestinationState::MediaPending
                } else if root.spec.metadata_enabled && !metadata {
                    DestinationState::MetadataPending
                } else {
                    DestinationState::Verified
                };
                result.destinations.insert(
                    encode(&(&key, &child, &manifest.decision.master, destination))?,
                    state,
                );
            }
        }
    }
    Ok(result)
}

impl SqliteStateDb {
    pub(crate) async fn recorded_selection_work(
        &self,
    ) -> Result<RecordedSelectionWork, StateError> {
        self.with_conn("reporting recorded selection work", move |conn| {
            // One read snapshot: retirement/publication cannot split the report.
            let tx = conn.unchecked_transaction()?;
            let mut result = RecordedSelectionWork::default();
            let mut profiles: BTreeMap<Vec<u8>, ProfileAccumulator> = BTreeMap::new();
            let ids: Vec<String> = tx
                .prepare("SELECT id FROM provider_active_generations ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for id in ids {
                let root = match load_generation(&tx, &id) {
                    Ok(root) => root,
                    Err(_) => {
                        result.invalid_roots += 1;
                        continue;
                    }
                };
                if root.spec.format == 1 {
                    result.historical_roots += 1;
                    continue;
                }
                if root
                    .spec
                    .profile
                    .get("current_download_metadata")
                    .and_then(serde_json::Value::as_u64)
                    != Some(1)
                {
                    result.unsupported_profiles += 1;
                    continue;
                }
                match read_root(&tx, &root) {
                    Ok(work) => profiles
                        .entry(encode(&(
                            &root.spec.scope,
                            &root.spec.config_hash,
                            &root.spec.profile,
                            root.spec.metadata_flags,
                        ))?)
                        .or_default()
                        .merge(work),
                    Err(_) => result.invalid_roots += 1,
                }
            }
            result.profiles = profiles
                .into_values()
                .map(ProfileAccumulator::finish)
                .collect();
            let ids: Vec<String> = tx
                .prepare("SELECT id FROM download_receipts ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for id in ids {
                match super::super::download_receipts::load(&tx, &id) {
                    Ok(Some(_)) => result.stored_media_receipts += 1,
                    _ => result.invalid_receipts += 1,
                }
            }
            let (current, historical, unclassified) =
                super::super::sparse_identity::recorded_observation_counts(&tx)?;
            result.current_source_observations = current;
            result.historical_source_observations = historical;
            result.unclassified_source_observations = unclassified;
            tx.commit()?;
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{DestinationState, ProfileAccumulator};

    #[test]
    fn recorded_work_deduplicates_roots_but_preserves_passes_and_pending_evidence() {
        let mut profile = ProfileAccumulator::default();
        let mut first = ProfileAccumulator {
            roots: 1,
            ..Default::default()
        };
        first.destinations.insert(
            b"album-a/resource/path".to_vec(),
            DestinationState::Verified,
        );
        first.destinations.insert(
            b"album-b/resource/path".to_vec(),
            DestinationState::Verified,
        );
        first.excluded.insert(b"excluded-child".to_vec());
        profile.merge(first);
        let mut repeated = ProfileAccumulator {
            roots: 1,
            unsealed_or_held_roots: 1,
            ..Default::default()
        };
        repeated.destinations.insert(
            b"album-a/resource/path".to_vec(),
            DestinationState::MediaPending,
        );
        repeated.destinations.insert(
            b"album-b/resource/path".to_vec(),
            DestinationState::MetadataPending,
        );
        repeated.excluded.insert(b"excluded-child".to_vec());
        repeated.deferred.insert(b"unknown-child".to_vec());
        profile.merge(repeated);
        let report = profile.finish();
        assert_eq!(report.roots, 2);
        assert_eq!(report.pending_media, 1);
        assert_eq!(report.pending_metadata, 1);
        assert_eq!(report.verified_destinations, 0);
        assert_eq!(report.excluded_decisions, 1);
        assert_eq!(report.deferred_decisions, 1);
        assert_eq!(report.unsealed_or_held_roots, 1);
    }
}
