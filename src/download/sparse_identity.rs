//! One library execution's sparse retry budget and completion receipts.

use chrono::{DateTime, Utc};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{DownloadConfig, DownloadRunMode, IncrementalDeltaSummary};
use crate::icloud::photos::asset::{ChangeEvent, SparseShareEvidence};
use crate::state::{SparseAttemptOutcome, SparseIdentity, SparseSourceId};
use crate::types::ChangeReason;

const SPARSE_LOOKUPS_PER_LIBRARY: usize = 100;

#[derive(Default)]
pub(super) struct SparseRetryContext {
    rows: FxHashMap<SparseSourceId, SparseIdentity>,
    due: FxHashSet<SparseSourceId>,
}

fn state_failure(summary: &mut IncrementalDeltaSummary) {
    summary.state_transition_failures += 1;
    summary.block_identity();
    tracing::warn!(
        diagnostic = "sparse_identity_state_failed",
        "Could not retain or validate sparse identity retry evidence"
    );
}

impl SparseRetryContext {
    pub(super) async fn prepare(
        events: &mut Vec<ChangeEvent>,
        config: &DownloadConfig,
        summary: &mut IncrementalDeltaSummary,
        run_mode: DownloadRunMode,
    ) -> Self {
        let mut context = Self::default();
        if !run_mode.downloads_files() {
            return context;
        }
        let Some(db) = &config.state_db else {
            return context;
        };
        let rows = match db.sparse_identities(&config.library).await {
            Ok(rows) => rows,
            Err(_) => {
                state_failure(summary);
                return context;
            }
        };
        // A replay/inventory bridge can omit an older source. Retained obligations
        // must still pass through the normal provider hydration and media planner.
        let seen: FxHashSet<_> = events
            .iter()
            .map(|event| event.record_name.clone())
            .collect();
        for row in rows {
            if SparseShareEvidence::from_durable_key(&row.original).is_none()
                || SparseShareEvidence::from_durable_key(&row.observed).is_none()
                || row
                    .lookup
                    .as_ref()
                    .is_some_and(|key| SparseShareEvidence::from_durable_key(key).is_none())
            {
                state_failure(summary);
                continue;
            }
            if !seen.contains(row.source.as_str()) {
                events.push(ChangeEvent {
                    record_name: row.source.as_str().into(),
                    record_type: Some("CPLAsset".into()),
                    master_record_name: None,
                    sparse_share: SparseShareEvidence::from_durable_key(&row.observed),
                    reason: ChangeReason::Created,
                    asset: None,
                    album: None,
                    relation: None,
                    token_unsafe_reason: None,
                });
            }
            context.rows.insert(row.source.clone(), row);
        }
        let now = Utc::now();
        for event in events.iter() {
            if event.reason != ChangeReason::Created
                || event.asset.is_some()
                || event.record_type.as_deref() != Some("CPLAsset")
            {
                continue;
            }
            let Some(key) = event
                .sparse_share
                .as_ref()
                .and_then(SparseShareEvidence::durable_key)
            else {
                continue;
            };
            let source = SparseSourceId::new(&event.record_name);
            if context
                .rows
                .get(&source)
                .is_some_and(|row| row.observed == key)
            {
                continue;
            }
            match db
                .observe_sparse_identity(&config.library, &source, &key, now)
                .await
            {
                Ok(row) => {
                    context.rows.insert(source, row);
                }
                Err(_) => state_failure(summary),
            }
        }
        context
    }

    // Select after authoritative mappings have been removed from the source-only
    // work. Otherwise recovered rows could monopolize a held library's budget.
    pub(super) fn select_due(&mut self, sources: &FxHashMap<String, Vec<usize>>) {
        let now = Utc::now();
        let mut due: Vec<_> = self
            .rows
            .values()
            .filter(|row| sources.contains_key(row.source.as_str()))
            .filter(|row| row.next_retry.is_none_or(|next| next <= now))
            .collect();
        due.sort_by(|left, right| {
            (
                left.next_retry
                    .or(left.last_attempt)
                    .unwrap_or(left.first_seen),
                left.generation,
                left.source.as_str(),
            )
                .cmp(&(
                    right
                        .next_retry
                        .or(right.last_attempt)
                        .unwrap_or(right.first_seen),
                    right.generation,
                    right.source.as_str(),
                ))
        });
        self.due = due
            .into_iter()
            .take(SPARSE_LOOKUPS_PER_LIBRARY)
            .map(|row| row.source.clone())
            .collect();
    }

    pub(super) fn permits(&self, source: &str, events: &[ChangeEvent], indices: &[usize]) -> bool {
        let key = SparseSourceId::new(source);
        let Some(row) = self.rows.get(&key) else {
            return true;
        };
        // Malformed/conflicting evidence is not a stable negative result.
        if indices.iter().any(|index| {
            events
                .get(*index)
                .and_then(|event| event.sparse_share.as_ref())
                .and_then(SparseShareEvidence::durable_key)
                .as_ref()
                != Some(&row.observed)
        }) {
            return true;
        }
        self.due.contains(&key)
    }

    pub(super) async fn record(
        &mut self,
        source: &str,
        outcome: SparseAttemptOutcome,
        config: &DownloadConfig,
        summary: &mut IncrementalDeltaSummary,
        run_mode: DownloadRunMode,
    ) {
        if !run_mode.downloads_files() {
            return;
        }
        let Some(db) = &config.state_db else {
            return;
        };
        let source = SparseSourceId::new(source);
        let now: DateTime<Utc> = Utc::now();
        if !self.rows.contains_key(&source)
            && let SparseAttemptOutcome::Unresolved(key) = &outcome
        {
            match db
                .observe_sparse_identity(&config.library, &source, key, now)
                .await
            {
                Ok(row) => {
                    self.rows.insert(source.clone(), row);
                }
                Err(_) => {
                    state_failure(summary);
                    return;
                }
            }
        }
        if let Some(row) = self.rows.get(&source) {
            match db.record_sparse_attempt(row, outcome, now).await {
                Ok(row) => {
                    self.rows.insert(source, row);
                }
                Err(_) => state_failure(summary),
            }
        }
    }

    pub(super) fn prove(&self, source: &str, summary: &mut IncrementalDeltaSummary) {
        if let Some(row) = self.rows.get(&SparseSourceId::new(source)) {
            let proof = row.proof();
            if !summary.sparse_identity_proofs.contains(&proof) {
                summary.sparse_identity_proofs.push(proof);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SPARSE_LOOKUPS_PER_LIBRARY, SparseRetryContext};
    use crate::download::{DownloadConfig, DownloadRunMode, IncrementalDeltaSummary};
    use crate::icloud::photos::asset::SparseShareEvidence;
    use crate::state::{
        SparseAttemptOutcome, SparseEvidence, SparseIdentityStore, SparseSourceId, SqliteStateDb,
    };
    use chrono::Utc;
    use std::sync::Arc;

    fn sources(
        events: &[crate::icloud::photos::asset::ChangeEvent],
    ) -> rustc_hash::FxHashMap<String, Vec<usize>> {
        events
            .iter()
            .enumerate()
            .map(|(index, event)| (event.record_name.to_string(), vec![index]))
            .collect()
    }

    fn key() -> SparseEvidence {
        SparseEvidence::new(
            r#"[1,"private-child","SharedSync-private-zone","private-owner"]"#.into(),
        )
    }

    #[tokio::test]
    async fn sparse_retry_budget_is_fair_and_replays_omitted_sources() {
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let config = DownloadConfig {
            library: "PrimarySync".into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let first = Utc::now();
        for index in 0..(SPARSE_LOOKUPS_PER_LIBRARY + 5) {
            db.observe_sparse_identity(
                "PrimarySync",
                &SparseSourceId::new(&format!("source-{index:03}")),
                &key(),
                first,
            )
            .await
            .unwrap();
        }
        let mut events = Vec::new();
        let mut summary = IncrementalDeltaSummary::default();
        let mut context = SparseRetryContext::prepare(
            &mut events,
            &config,
            &mut summary,
            DownloadRunMode::Download,
        )
        .await;
        context.select_due(&sources(&events));
        assert_eq!(events.len(), 105);
        assert_eq!(context.due.len(), 100);
        for index in 0..events.len() {
            let source = events[index].record_name.as_ref();
            assert_eq!(context.permits(source, &events, &[index]), index < 100);
            if index < 100 {
                // Even inconclusive (no backoff) results yield to older waiting work.
                context
                    .record(
                        source,
                        SparseAttemptOutcome::Inconclusive,
                        &config,
                        &mut summary,
                        DownloadRunMode::Download,
                    )
                    .await;
            }
        }
        let mut replay = Vec::new();
        let mut next = SparseRetryContext::prepare(
            &mut replay,
            &config,
            &mut summary,
            DownloadRunMode::Download,
        )
        .await;
        next.select_due(&sources(&replay));
        for index in 100..105 {
            assert!(
                next.due
                    .contains(&SparseSourceId::new(&format!("source-{index:03}")))
            );
        }
        let only_unmapped = sources(&replay)
            .into_iter()
            .filter(|(source, _)| source.as_str() >= "source-100")
            .collect();
        next.select_due(&only_unmapped);
        assert_eq!(next.due.len(), 5);
        assert!(!summary.identity_incomplete);
        assert_eq!(
            db.get_summary().await.unwrap().unresolved_sparse_records,
            105
        );
    }

    #[tokio::test]
    async fn sparse_retry_defers_stable_evidence_but_not_malformed_or_changed() {
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let config = DownloadConfig {
            library: "PrimarySync".into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let row = db
            .observe_sparse_identity(
                "PrimarySync",
                &SparseSourceId::new("source"),
                &key(),
                Utc::now(),
            )
            .await
            .unwrap();
        db.record_sparse_attempt(&row, SparseAttemptOutcome::Unresolved(key()), Utc::now())
            .await
            .unwrap();
        let mut events = Vec::new();
        let mut summary = IncrementalDeltaSummary::default();
        let mut context = SparseRetryContext::prepare(
            &mut events,
            &config,
            &mut summary,
            DownloadRunMode::Download,
        )
        .await;
        context.select_due(&sources(&events));
        assert_eq!(events.len(), 1);
        assert!(!context.permits("source", &events, &[0]));
        assert_eq!(db.get_summary().await.unwrap().deferred_sparse_records, 1);
        events[0].sparse_share = Some(SparseShareEvidence::Malformed);
        assert!(context.permits("source", &events, &[0]));
        events[0].sparse_share = SparseShareEvidence::from_durable_key(&SparseEvidence::new(
            r#"[1,"different-child","SharedSync-private-zone","private-owner"]"#.into(),
        ));
        let mut changed = SparseRetryContext::prepare(
            &mut events,
            &config,
            &mut summary,
            DownloadRunMode::Download,
        )
        .await;
        changed.select_due(&sources(&events));
        assert!(changed.permits("source", &events, &[0]));
        assert_eq!(db.get_summary().await.unwrap().deferred_sparse_records, 0);
        assert_eq!(
            db.sparse_identities("PrimarySync").await.unwrap()[0].original,
            key()
        );
    }

    #[tokio::test]
    async fn sparse_retry_read_only_modes_do_not_mutate_state() {
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let config = DownloadConfig {
            library: "PrimarySync".into(),
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        for mode in [DownloadRunMode::DryRun, DownloadRunMode::PrintFilenames] {
            let mut events = Vec::new();
            let mut summary = IncrementalDeltaSummary::default();
            let mut context =
                SparseRetryContext::prepare(&mut events, &config, &mut summary, mode).await;
            context
                .record(
                    "source",
                    SparseAttemptOutcome::Unresolved(key()),
                    &config,
                    &mut summary,
                    mode,
                )
                .await;
            assert!(events.is_empty());
            assert!(
                db.sparse_identities("PrimarySync")
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(db.get_summary().await.unwrap().unresolved_identity_zones, 0);
        }
    }
}
