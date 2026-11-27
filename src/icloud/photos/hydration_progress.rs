//! Task-scoped, anonymous diagnostics for best-effort album hydration reads.
//! No identities or provider errors enter this state. It has no policy authority.

use std::cell::RefCell;
use tokio::time::Instant;

tokio::task_local! {
    static PROGRESS: RefCell<Progress>;
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
mod tests {}
