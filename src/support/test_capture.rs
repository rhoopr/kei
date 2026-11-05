//! Test-only in-memory capture of the production diagnostic tracing bridge.
//! No queue, worker, file, lock, staging, or rotation is involved.
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;

use super::privacy;

#[derive(Clone, Default)]
pub(crate) struct TestCapture {
    events: Arc<Mutex<Vec<Value>>>,
}

#[derive(Default)]
struct Visitor {
    kind: Option<String>,
    fields: Map<String, Value>,
}

impl TestCapture {
    fn push(&self, event: Value) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    pub(crate) fn drain(&self) -> Vec<Value> {
        self.events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }
}

pub(crate) fn test_layer() -> (TestCapture, impl Fn() -> Vec<Value>) {
    let capture = TestCapture::default();
    let drain = capture.clone();
    (capture, move || drain.drain())
}

impl Visit for Visitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "diagnostic" {
            if privacy::contract(value).is_some() {
                self.kind = Some(value.into());
            }
        } else if privacy::fixed_label(value)
            || (field.name() == "child_reference_status"
                && privacy::is_child_reference_status(value))
        {
            self.fields.insert(field.name().into(), value.into());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().into(), value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().into(), value.into());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().into(), value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.fields.insert(field.name().into(), Value::Number(n));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "support_fields" {
            let rendered = format!("{value:?}");
            if rendered.len() <= 64 * 1024
                && let Ok(Value::Object(fields)) = serde_json::from_str(&rendered)
            {
                self.fields.extend(fields);
            }
            return;
        }
        if matches!(field.name(), "oldest_refreshed_url_observed_age_secs") {
            let rendered = format!("{value:?}");
            if let Some(number) = rendered
                .strip_prefix("Some(")
                .and_then(|s| s.strip_suffix(')'))
                .and_then(|s| s.parse::<f64>().ok())
            {
                self.record_f64(field, number);
            }
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for TestCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !event
            .metadata()
            .fields()
            .iter()
            .any(|f| f.name() == "diagnostic")
        {
            return;
        }
        let mut visitor = Visitor::default();
        event.record(&mut visitor);
        if let Some(kind) = visitor.kind
            && let Some(fields) = privacy::diagnostic(&kind, &visitor.fields)
        {
            self.push(json!({"kind": kind, "fields": fields}));
        }
    }
}
