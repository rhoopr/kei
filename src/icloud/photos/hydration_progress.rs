//! Task-scoped, anonymous diagnostics for best-effort album hydration reads.
//! No identities or provider errors enter this state. It has no policy authority.

use std::cell::RefCell;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

const INTERVAL: Duration = Duration::from_secs(30);

tokio::task_local! {
    static PROGRESS: RefCell<Progress>;
}

pub(super) enum Phase {
    AssetLookup,
    PairedLookup,
    Changes,
}

struct Progress {
    started: Instant,
    waiting_since: Instant,
    source_index: usize,
    phase: &'static str,
    page: usize,
    pages_received: usize,
    records_received: usize,
    requests: usize,
    attempt: usize,
    retries: usize,
    request_state: &'static str,
    outcome: &'static str,
}

impl Progress {
    fn emit(&self, outcome: &'static str) {
        tracing::info!(target: "kei::icloud::photos::album",
            outcome,
            phase = self.phase,
            source_index = self.source_index,
            source_role = match self.source_index { 0 => "none", 1 => "owner", _ => "fallback" },
            page = self.page,
            pages_received = self.pages_received,
            records_received = self.records_received,
            requests = self.requests,
            attempt = self.attempt,
            retries = self.retries,
            request_state = self.request_state,
            wait_secs = self.waiting_since.elapsed().as_secs(),
            elapsed_secs = self.started.elapsed().as_secs(),
            "Album hydration progress");
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.emit(self.outcome);
    }
}

fn update(f: impl FnOnce(&mut Progress)) {
    // Other provider operations, including spawned tasks, have no active scope.
    let _ = PROGRESS.try_with(|progress| f(&mut progress.borrow_mut()));
}

pub(super) async fn observe<T>(read: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    let now = Instant::now();
    let progress = Progress {
        started: now,
        waiting_since: now,
        source_index: 0,
        phase: "starting",
        page: 0,
        pages_received: 0,
        records_received: 0,
        requests: 0,
        attempt: 0,
        retries: 0,
        request_state: "idle",
        outcome: "interrupted",
    };
    PROGRESS
        .scope(RefCell::new(progress), async {
            let mut ticks = tokio::time::interval_at(now + INTERVAL, INTERVAL);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tokio::pin!(read);
            loop {
                tokio::select! {
                    biased;
                    result = &mut read => {
                        update(|p| p.outcome = if result.is_ok() { "returned" } else { "error" });
                        return result;
                    }
                    _ = ticks.tick() => update(|p| p.emit("waiting")),
                }
            }
        })
        .await
}

pub(super) fn source(index: usize) {
    update(|p| {
        p.source_index = index;
        p.page = 0;
    });
}

pub(super) fn phase(phase: Phase) {
    update(|p| {
        p.phase = match phase {
            Phase::AssetLookup => "asset_lookup",
            Phase::PairedLookup => "paired_lookup",
            Phase::Changes => "changes",
        };
        p.request_state = "idle";
        p.waiting_since = Instant::now();
    });
}

pub(super) fn page_started() {
    update(|p| p.page = p.page.saturating_add(1));
}

pub(super) fn page_received(records: usize) {
    update(|p| {
        p.pages_received = p.pages_received.saturating_add(1);
        p.records_received = p.records_received.saturating_add(records);
    });
}

pub(super) fn request_started() {
    update(|p| {
        p.requests = p.requests.saturating_add(1);
        p.attempt = 0;
    });
}

pub(super) fn attempt_started() {
    update(|p| {
        p.attempt = p.attempt.saturating_add(1);
        p.retries = p.retries.saturating_add(usize::from(p.attempt > 1));
        p.request_state = "request";
        p.waiting_since = Instant::now();
    });
}

pub(super) fn attempt_finished() {
    update(|p| {
        p.request_state = "between_attempts";
        p.waiting_since = Instant::now();
    });
}

pub(super) fn request_finished() {
    update(|p| {
        p.request_state = "idle";
        p.waiting_since = Instant::now();
    });
}

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn issue958_progress_scope_is_not_inherited_by_spawned_requests() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        super::observe(async {
            tokio::spawn(async {
                super::request_started();
                super::attempt_started();
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                super::attempt_finished();
                super::request_finished();
            })
            .await?;
            Ok(())
        })
        .await
        .unwrap();
        let events: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|e| e.message() == Some("Album hydration progress"))
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].field("outcome"), Some("returned"));
        assert_eq!(events[0].field("requests"), Some("0"));
        assert_eq!(events[0].field("attempt"), Some("0"));
    }
}
