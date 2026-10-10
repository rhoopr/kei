//! Bounded support evidence, independent of logging filters and backup policy.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc,
};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;

use super::privacy;

pub(super) const MAX_BYTES: u64 = 512 * 1024;
pub(super) const MAX_CYCLES: usize = 16;
pub(super) const MAX_GROUPS: usize = 128;
const QUEUE: usize = 256;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct History {
    #[serde(skip)]
    active_generation: Option<u64>,
    pub schema_version: u32,
    pub cycles_evicted: u64,
    pub queue_dropped: u64,
    pub groups_omitted: u64,
    pub previous_history_unavailable: bool,
    pub cycles: Vec<Cycle>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Cycle {
    pub id: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub outcome: String,
    pub build_version: String,
    #[serde(default)]
    pub build_revision: Option<String>,
    #[serde(default)]
    pub build_dirty: Option<bool>,
    pub configuration: Value,
    pub stats: Value,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Diagnostic {
    pub kind: String,
    pub fields: Map<String, Value>,
    pub observations: u64,
    pub first_at: String,
    pub last_at: String,
}

pub(super) fn read(path: &Path) -> (History, &'static str) {
    let Ok(file) = std::fs::File::open(path) else {
        return (History::default(), "unavailable");
    };
    let mut bytes = Vec::new();
    if file.take(MAX_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_BYTES {
        return (History::default(), "truncated_or_unreadable");
    }
    let Ok(mut history) = serde_json::from_slice::<History>(&bytes) else {
        return (History::default(), "invalid_or_interrupted");
    };
    if history.schema_version != 1 {
        return (History::default(), "unsupported_schema");
    }
    // Re-apply the contract to untrusted files. Never export an arbitrary field,
    // version, label or timestamp merely because a previous writer saved it.
    let original_cycles = history.cycles.len();
    history.cycles.retain(|cycle| {
        uuid::Uuid::parse_str(&cycle.id).is_ok()
            && utc(&cycle.started_at).is_some()
            && cycle.completed_at.as_ref().is_none_or(|v| utc(v).is_some())
    });
    history.previous_history_unavailable |= history.cycles.len() != original_cycles;
    for cycle in &mut history.cycles {
        cycle.started_at = utc(&cycle.started_at).unwrap_or_default();
        cycle.completed_at = cycle.completed_at.as_deref().and_then(utc);
        if !privacy::fixed_label(&cycle.outcome) {
            cycle.outcome = "unknown".into();
        }
        // Versions from stored files are not trusted build identities.
        if !valid_version(&cycle.build_version) {
            cycle.build_version = "unavailable".into();
        }
        if cycle
            .build_revision
            .as_ref()
            .is_some_and(|v| v.len() != 40 || !v.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            cycle.build_revision = None;
        }
        cycle.configuration = privacy::configuration(&cycle.configuration);
        cycle.stats = privacy::stats(&cycle.stats);
        let original_groups = cycle.diagnostics.len();
        cycle.diagnostics.retain(|d| {
            privacy::contract(&d.kind).is_some()
                && utc(&d.first_at).is_some()
                && utc(&d.last_at).is_some()
        });
        history.groups_omitted = history.groups_omitted.saturating_add(
            u64::try_from(original_groups - cycle.diagnostics.len()).unwrap_or(u64::MAX),
        );
        for d in &mut cycle.diagnostics {
            d.fields = privacy::diagnostic(&d.kind, &d.fields).unwrap_or_default();
            d.first_at = utc(&d.first_at).unwrap_or_default();
            d.last_at = utc(&d.last_at).unwrap_or_default();
        }
        if cycle.diagnostics.len() > MAX_GROUPS {
            history.groups_omitted = history.groups_omitted.saturating_add(
                u64::try_from(cycle.diagnostics.len() - MAX_GROUPS).unwrap_or(u64::MAX),
            );
            cycle.diagnostics.truncate(MAX_GROUPS);
        }
    }
    let mut aliases = std::collections::HashMap::new();
    for cycle in &mut history.cycles {
        for d in &mut cycle.diagnostics {
            for key in ["item_alias", "scope_alias"] {
                if let Some(value) = d.fields.get_mut(key) {
                    let ordinal = aliases.len() + 1;
                    if let Some(original) = value.as_str() {
                        let alias = aliases
                            .entry(original.to_owned())
                            .or_insert_with(|| format!("alias-{ordinal}"));
                        *value = Value::String(alias.clone());
                    }
                }
            }
        }
    }
    trim(&mut history);
    (history, "available")
}

fn valid_version(value: &str) -> bool {
    let core = value.strip_suffix("-dev").unwrap_or(value);
    let parts: Vec<_> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|c| c.is_ascii_digit()))
}

fn utc(value: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|v| v.with_timezone(&chrono::Utc).to_rfc3339())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn trim(history: &mut History) {
    while history.cycles.len() > MAX_CYCLES {
        history.cycles.remove(0);
        history.cycles_evicted = history.cycles_evicted.saturating_add(1);
    }
}

enum Message {
    Tagged(u64, Box<Message>),
    Start(Value),
    Begin(Value),
    Complete(Value, &'static str),
    Observe(&'static str, Map<String, Value>),
    Event(String, Map<String, Value>),
    Stop,
}

#[derive(Clone)]
pub(crate) struct Recorder {
    sender: mpsc::SyncSender<Message>,
    generation: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    aliases: Arc<Mutex<std::collections::HashMap<String, String>>>,
    alias_namespace: uuid::Uuid,
}

impl Recorder {
    fn tagged(&self, message: Message) -> Message {
        let generation = if matches!(message, Message::Start(_) | Message::Begin(_)) {
            self.generation.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            self.generation.load(Ordering::SeqCst)
        };
        Message::Tagged(generation, Box::new(message))
    }

    fn send(&self, message: Message) {
        if self.sender.try_send(self.tagged(message)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

static ACTIVE: OnceLock<Recorder> = OnceLock::new();

pub(crate) fn begin(configuration: Value) {
    if let Some(recorder) = ACTIVE.get() {
        recorder.send(Message::Begin(privacy::configuration(&configuration)));
    }
}

pub(crate) fn complete(stats: &crate::download::SyncStats, outcome: &'static str) {
    if let Some(recorder) = ACTIVE.get()
        && let Ok(value) = serde_json::to_value(stats)
    {
        recorder.send(Message::Complete(privacy::stats(&value), outcome));
    }
}

pub(crate) fn observe(kind: &'static str, fields: Value) {
    if let Some(recorder) = ACTIVE.get()
        && let Some(fields) = fields
            .as_object()
            .and_then(|v| privacy::diagnostic(kind, v))
    {
        recorder.send(Message::Observe(kind, fields));
    }
}

/// Correlation identities live only in a bounded process-local map. Saved
/// aliases contain a random namespace and ordinal, never a provider-derived hash.
/// A restart starts a new namespace; the export renumbers aliases locally.
pub(crate) fn observe_scoped(
    scope: &str,
    item: Option<(&str, &str)>,
    kind: &'static str,
    mut fields: Value,
) {
    let Some(recorder) = ACTIVE.get() else {
        return;
    };
    let key = if let Some((id, version)) = item {
        format!("item:{scope}:{id}:{version}")
    } else {
        format!("scope:{scope}")
    };
    if let Ok(mut aliases) = recorder.aliases.lock() {
        let ordinal = aliases.len() + 1;
        let alias = if let Some(alias) = aliases.get(&key) {
            Some(alias.clone())
        } else if aliases.len() < MAX_GROUPS
            && (item.is_none()
                || fields.get("operation").and_then(Value::as_str) != Some("publication")
                || fields
                    .get("publication_time_retained")
                    .and_then(Value::as_bool)
                    == Some(true))
        {
            let alias = format!("{}/{ordinal}", recorder.alias_namespace);
            aliases.insert(key, alias.clone());
            Some(alias)
        } else {
            None
        };
        if let Some(map) = fields.as_object_mut() {
            if let Some(alias) = alias {
                map.insert(
                    if item.is_some() {
                        "item_alias"
                    } else {
                        "scope_alias"
                    }
                    .into(),
                    Value::String(alias),
                );
            } else if map.get("operation").and_then(Value::as_str) != Some("publication") {
                map.insert("correlation_unavailable".into(), Value::Bool(true));
            }
        }
    }
    observe(kind, fields);
}

pub(crate) struct Guard {
    recorder: Recorder,
    worker: std::thread::JoinHandle<()>,
}

impl Guard {
    pub(crate) async fn finish(self) {
        // A blocking queue handoff joins the writer after every accepted event.
        // File work and joining never block Tokio's executor threads.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = self
                .recorder
                .sender
                .send(self.recorder.tagged(Message::Stop));
            let _ = self.worker.join();
        })
        .await;
    }
}

pub(crate) fn start(path: PathBuf, configuration: Value) -> Option<(Recorder, Guard)> {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    let recorder = Recorder {
        sender,
        generation: Arc::new(AtomicU64::new(0)),
        dropped: Arc::new(AtomicU64::new(0)),
        aliases: Arc::new(Mutex::new(std::collections::HashMap::new())),
        alias_namespace: uuid::Uuid::new_v4(),
    };
    let dropped = Arc::clone(&recorder.dropped);
    let worker = std::thread::Builder::new()
        .name("kei-support-history".into())
        .spawn(move || {
            let Some(parent) = path.parent() else {
                return;
            };
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
            let lock_path = path.with_extension("support-lock");
            let Ok(lock) = private_options()
                .create(true)
                .truncate(false)
                .open(&lock_path)
            else {
                return;
            };
            if !matches!(lock.try_lock_exclusive(), Ok(true)) {
                return;
            }
            let (mut history, status) = read(&path);
            history.schema_version = 1;
            history.previous_history_unavailable |= !matches!(status, "available" | "unavailable");
            let mut dirty = false;
            let mut last_save = std::time::Instant::now();
            let flush_interval = std::time::Duration::from_millis(500);
            loop {
                let mut stopping = false;
                match receiver.recv_timeout(flush_interval) {
                    Ok(first) => {
                        dirty = true;
                        stopping = apply(&mut history, first);
                        for _ in 1..QUEUE {
                            if stopping {
                                break;
                            }
                            match receiver.try_recv() {
                                Ok(message) => stopping = apply(&mut history, message),
                                Err(_) => break,
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => stopping = true,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if dirty && (stopping || last_save.elapsed() >= flush_interval) {
                    history.queue_dropped = history
                        .queue_dropped
                        .saturating_add(dropped.swap(0, Ordering::Relaxed));
                    // Coalesce optional file I/O, without backpressure on backup work.
                    let _ = save(&path, &mut history);
                    dirty = false;
                    last_save = std::time::Instant::now();
                }
                if stopping {
                    break;
                }
            }
        })
        .ok()?;
    let _ = ACTIVE.set(recorder.clone());
    recorder.send(Message::Start(privacy::configuration(&configuration)));
    Some((recorder.clone(), Guard { recorder, worker }))
}

fn apply(history: &mut History, message: Message) -> bool {
    match message {
        Message::Tagged(generation, message) => {
            if matches!(*message, Message::Start(_) | Message::Begin(_)) {
                history.active_generation = Some(generation);
            } else if history.active_generation != Some(generation) {
                // Losing a Begin omits that cycle rather than attributing its
                // events or completion to the previous (possibly completed) one.
                history.queue_dropped = history.queue_dropped.saturating_add(1);
                return matches!(*message, Message::Stop);
            }
            return apply(history, *message);
        }
        Message::Start(configuration) => begin_record(history, configuration, false),
        Message::Begin(configuration) => begin_record(history, configuration, true),
        Message::Complete(stats, outcome) => {
            if let Some(cycle) = history.cycles.last_mut()
                && cycle.completed_at.is_none()
            {
                cycle.completed_at = Some(now());
                cycle.outcome = outcome.into();
                cycle.stats = stats;
            }
        }
        Message::Observe(kind, fields) => add(history, kind, fields),
        Message::Event(kind, fields) => add(history, &kind, fields),
        Message::Stop => {
            if let Some(cycle) = history.cycles.last_mut()
                && cycle.completed_at.is_none()
                && let Some(outcome) = cycle
                    .diagnostics
                    .iter()
                    .rev()
                    .find(|d| {
                        d.kind == "support_startup_v1"
                            && d.fields.get("phase").and_then(Value::as_str) == Some("shutdown")
                    })
                    .and_then(|d| d.fields.get("outcome"))
                    .and_then(Value::as_str)
            {
                cycle.outcome = outcome.to_owned();
                cycle.completed_at = Some(now());
            }
            return true;
        }
    }
    false
}

fn begin_record(history: &mut History, configuration: Value, complete_startup: bool) {
    // Entering the next cycle proves startup reached normal operation.
    // A process that stops before this handoff retains an unfinished record.
    if complete_startup
        && let Some(previous) = history.cycles.last_mut()
        && previous.completed_at.is_none()
        && previous
            .diagnostics
            .iter()
            .any(|d| d.kind == "support_startup_v1")
    {
        previous.completed_at = Some(now());
        previous.outcome = "success".into();
    }
    history.cycles.push(Cycle {
        id: uuid::Uuid::new_v4().to_string(),
        started_at: now(),
        completed_at: None,
        outcome: "running".into(),
        build_version: env!("CARGO_PKG_VERSION").into(),
        build_revision: option_env!("KEI_BUILD_REVISION").map(str::to_owned),
        build_dirty: option_env!("KEI_BUILD_DIRTY").and_then(|v| v.parse().ok()),
        configuration,
        stats: Value::Object(Map::new()),
        diagnostics: Vec::new(),
    });
    trim(history);
}

fn add(history: &mut History, kind: &str, fields: Map<String, Value>) {
    let Some(cycle) = history.cycles.last_mut() else {
        return;
    };
    let time = now();
    if let Some(existing) = cycle
        .diagnostics
        .iter_mut()
        .find(|d| d.kind == kind && d.fields == fields)
    {
        existing.observations = existing.observations.saturating_add(1);
        existing.last_at = time;
    } else if cycle.diagnostics.len() < MAX_GROUPS {
        cycle.diagnostics.push(Diagnostic {
            kind: kind.into(),
            fields,
            observations: 1,
            first_at: time.clone(),
            last_at: time,
        });
    } else {
        history.groups_omitted = history.groups_omitted.saturating_add(1);
    }
}

fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn save(path: &Path, history: &mut History) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(history)?;
    while bytes.len() as u64 > MAX_BYTES && history.cycles.len() > 1 {
        history.cycles.remove(0);
        history.cycles_evicted = history.cycles_evicted.saturating_add(1);
        bytes = serde_json::to_vec(history)?;
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Err(std::io::Error::other("support history size limit"));
    }
    let temporary = path.with_extension("support-tmp");
    // One owned staging name bounds interrupted writes across restarts. Removing
    // the entry cannot follow a symlink; create_new rejects replacement races.
    match std::fs::symlink_metadata(&temporary) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
            std::fs::remove_file(&temporary)?;
        }
        Ok(_) => return Err(std::io::Error::other("support staging path unavailable")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = private_options().create_new(true).open(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        crate::fs_util::atomic_install(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

// Only allowlisted fixed labels and scalar fields are visited. Messages, IDs,
// paths, tokens, provider strings, responses and arbitrary errors are ignored.
#[derive(Default)]
struct Visitor {
    kind: Option<String>,
    fields: Map<String, Value>,
}
impl Visit for Visitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "diagnostic" {
            if privacy::contract(value).is_some() {
                self.kind = Some(value.into());
            }
        } else if privacy::fixed_label(value) {
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
        // Formatting is allowed only for explicitly typed enum/optional scalar
        // fields. Other debug values can carry arbitrary private data.
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
impl<S: tracing::Subscriber> Layer<S> for Recorder {
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
            self.send(Message::Event(kind, fields));
        }
    }
}

#[cfg(test)]
mod tests;

/// Production tracing bridge exercised by disposable provider fixtures.
#[cfg(test)]
pub(crate) fn test_layer() -> (Recorder, impl Fn() -> Vec<Value>) {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    let recorder = Recorder {
        sender,
        generation: Arc::new(AtomicU64::new(0)),
        dropped: Arc::new(AtomicU64::new(0)),
        aliases: Arc::new(Mutex::new(std::collections::HashMap::new())),
        alias_namespace: uuid::Uuid::new_v4(),
    };
    (recorder, move || {
        receiver
            .try_iter()
            .filter_map(|message| match message {
                Message::Tagged(_, message) => match *message {
                    Message::Event(kind, fields) => {
                        Some(serde_json::json!({"kind":kind,"fields":fields}))
                    }
                    _ => None,
                },
                Message::Event(kind, fields) => {
                    Some(serde_json::json!({"kind":kind,"fields":fields}))
                }
                _ => None,
            })
            .collect()
    })
}
