//! Bounded, invocation-local token relationships. Never consulted by sync policy.
use std::sync::{Arc, Mutex};
use std::task::Poll;

use futures_util::stream;
use serde_json::{Value, json};

use super::{DownloadPhotoStream, PassTokenObservation, PassTokenResult};
use crate::commands::{AlbumPass, PassKind};
use crate::support::token_equivalence::MAX_PASSES;

const MAX_TOKEN_BYTES: usize = 4096;

#[derive(Clone)]
pub(super) struct CompletionOrder(Arc<Mutex<(usize, [Option<usize>; MAX_PASSES])>>);

impl Default for CompletionOrder {
    fn default() -> Self {
        Self(Arc::new(Mutex::new((0, [None; MAX_PASSES]))))
    }
}

impl CompletionOrder {
    /// Observes stream drainage, not HTTP completion, token arrival or media writes.
    /// Streams dropped before drainage have no order. Filtered drainage is not
    /// proof of natural provider EOF. No extra poll or task is introduced.
    pub(super) fn wrap(
        &self,
        mut source: DownloadPhotoStream,
        index: usize,
    ) -> DownloadPhotoStream {
        let order = self.clone();
        let mut recorded = false;
        Box::pin(stream::poll_fn(move |cx| {
            let result = source.as_mut().poll_next(cx);
            if matches!(result, Poll::Ready(None)) && !recorded {
                recorded = true;
                if let Ok(mut state) = order.0.lock() {
                    state.0 = state.0.saturating_add(1);
                    let ordinal = state.0;
                    if let Some(slot) = state.1.get_mut(index) {
                        *slot = Some(ordinal);
                    }
                }
            }
            result
        }))
    }

    fn get(&self, index: usize) -> Option<usize> {
        self.0.lock().ok()?.1.get(index).copied().flatten()
    }
}

pub(super) struct TokenDiagnostics {
    // Raw tokens stay in this bounded invocation-local table and never enter JSON.
    tokens: Vec<String>,
    rows: Vec<Value>,
    total: usize,
    pub(super) initial_order: CompletionOrder,
    pub(super) recovery_order: CompletionOrder,
}

impl TokenDiagnostics {
    pub(super) fn new(passes: &[AlbumPass], selected_zone: &str) -> Self {
        let rows = passes.iter().take(MAX_PASSES).enumerate().map(|(index, pass)| {
            let (query_kind, scope) = pass.album.token_diagnostic_categories(&passes.first().unwrap_or(pass).album, selected_zone);
            json!({
                "pass": index + 1,
                "pass_kind": match pass.kind { PassKind::Album => "album", PassKind::SmartFolder => "smart_folder", PassKind::Unfiled => "unfiled" },
                "query_kind": query_kind, "scope": scope,
                "initial_status": "not_observed", "initial_group": null, "initial_completion_order": null,
                "recovery_status": "not_retried", "recovery_group": null, "recovery_completion_order": null,
            })
        }).collect();
        Self {
            tokens: Vec::new(),
            rows,
            total: passes.len(),
            initial_order: CompletionOrder::default(),
            recovery_order: CompletionOrder::default(),
        }
    }

    pub(super) fn recovery_requested(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(index) {
            row["recovery_status"] = json!("not_observed");
        }
    }

    pub(super) fn record(&mut self, observation: &PassTokenObservation, recovery: bool) {
        let index = observation.pass.index;
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        let (status, group) = match &observation.result {
            PassTokenResult::Present(token) if token.len() <= MAX_TOKEN_BYTES => {
                let group = if let Some(index) = self.tokens.iter().position(|known| known == token)
                {
                    Some(index + 1)
                } else if self.tokens.len() < MAX_PASSES * 2 {
                    self.tokens.push(token.clone());
                    Some(self.tokens.len())
                } else {
                    None
                };
                (
                    if group.is_some() {
                        "present"
                    } else {
                        "group_limit"
                    },
                    group,
                )
            }
            PassTokenResult::Present(_) => ("token_too_large", None),
            PassTokenResult::Missing => ("missing", None),
            PassTokenResult::Blank => ("blank", None),
            PassTokenResult::ReceiverDropped => ("receiver_dropped", None),
            PassTokenResult::EnumerationIncomplete => ("enumeration_incomplete", None),
        };
        let (status_key, group_key, order_key, order) = if recovery {
            (
                "recovery_status",
                "recovery_group",
                "recovery_completion_order",
                &self.recovery_order,
            )
        } else {
            (
                "initial_status",
                "initial_group",
                "initial_completion_order",
                &self.initial_order,
            )
        };
        row[status_key] = json!(status);
        row[group_key] = json!(group);
        row[order_key] = json!(order.get(index));
    }

    pub(super) fn fields(&self, phase: &'static str) -> Value {
        json!({ "phase": phase, "passes_total": self.total,
            "passes_omitted": self.total.saturating_sub(self.rows.len()), "rows": self.rows })
    }

    pub(super) fn emit(&self, zone: &str, phase: &'static str) {
        crate::support::observe_scoped(
            zone,
            None,
            "support_token_equivalence_v1",
            self.fields(phase),
        );
    }
}

#[cfg(test)]
mod tests;
