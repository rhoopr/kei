//! Fetcher completion evidence and unanimous token capture.

use rustc_hash::FxHashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::task::JoinHandle;

/// Await all fetcher handles, logging and returning `true` if any panicked.
pub(super) async fn await_fetcher_handles(handles: Vec<JoinHandle<()>>) -> bool {
    let mut panicked = false;
    for handle in handles {
        if let Err(e) = handle.await
            && e.is_panic()
        {
            tracing::error!(target: "kei::icloud::photos::album", error = ?e, "Photo fetcher task panicked");
            panicked = true;
        }
    }
    panicked
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnumerationFailure {
    FetcherError,
    ConsumerDropped,
    MalformedRecord,
    UnpairedRecords,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnumerationCompletion {
    ProvenEof,
    UserBoundReached,
    Incomplete(EnumerationFailure),
}

/// Return a full-enumeration sync token only when every fetcher that reported
/// one agreed. A single overwritten token is unsafe: if two parallel fetchers
/// observed different zone tokens, advancing either token could skip records
/// that were not present in the other fetcher's snapshot.
fn unanimous_fetcher_sync_token(album: &str, tokens: &[String]) -> Option<String> {
    let first = tokens.first()?;
    if tokens.iter().all(|token| token == first) {
        return Some(first.clone());
    }

    let mut unique_tokens = FxHashSet::default();
    for token in tokens {
        unique_tokens.insert(token.as_str());
    }
    tracing::warn!(target: "kei::icloud::photos::album",
        album,
        token_count = tokens.len(),
        unique_token_count = unique_tokens.len(),
        "Full enumeration syncToken mismatch across parallel fetchers; \
         blocking sync token advancement"
    );
    None
}

#[derive(Debug, Default)]
pub(super) struct FetcherSyncTokenCapture {
    observations: tokio::sync::Mutex<Vec<(Option<String>, EnumerationCompletion)>>,
    expected_fetchers: AtomicUsize,
    completed_fetchers: AtomicUsize,
    suppressed: AtomicBool,
}

impl FetcherSyncTokenCapture {
    pub(super) fn expect(&self, fetchers: usize) {
        self.expected_fetchers.store(fetchers, Ordering::Relaxed);
    }

    pub(super) async fn complete(&self, token: Option<String>, completion: EnumerationCompletion) {
        self.observations.lock().await.push((token, completion));
        self.completed_fetchers.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn suppress(&self) {
        self.suppressed.store(true, Ordering::Relaxed);
    }

    pub(super) async fn resolve(&self, album: &str) -> Option<String> {
        if self.suppressed.load(Ordering::Relaxed) {
            tracing::debug!(target: "kei::icloud::photos::album",
                album,
                "Full enumeration stopped at the caller's limit; syncToken is not a complete-zone checkpoint"
            );
            return None;
        }

        let expected = self.expected_fetchers.load(Ordering::Relaxed);
        let completed = self.completed_fetchers.load(Ordering::Relaxed);
        if completed != expected {
            tracing::warn!(target: "kei::icloud::photos::album",
                album,
                expected_fetchers = expected,
                completed_fetchers = completed,
                "Full enumeration did not receive completion evidence from every fetcher; blocking sync token advancement"
            );
            return None;
        }

        let observations = self.observations.lock().await;
        if let Some(failure) = observations
            .iter()
            .find_map(|(_, completion)| match completion {
                EnumerationCompletion::Incomplete(failure) => Some(*failure),
                _ => None,
            })
        {
            tracing::warn!(target: "kei::icloud::photos::album",
                album,
                ?failure,
                "Full enumeration was incomplete in a fetcher"
            );
            return None;
        }
        if observations
            .iter()
            .any(|(_, completion)| matches!(completion, EnumerationCompletion::UserBoundReached))
        {
            tracing::debug!(target: "kei::icloud::photos::album", album, "Full enumeration stopped at a user bound");
            return None;
        }
        let present = observations
            .iter()
            .filter_map(|(token, _)| token.as_ref())
            .cloned()
            .collect::<Vec<_>>();
        // Completion is required from every fetcher, but CloudKit may omit a
        // syncToken on an empty tail page. In that case the unanimous token
        // observed by the completed data fetchers is still the pass token;
        // an incomplete fetcher is rejected by the count above.
        unanimous_fetcher_sync_token(album, &present)
    }
}

#[cfg(test)]
mod tests;
