use super::{
    History, MAX_BYTES, MAX_CYCLES, MAX_GROUPS, Message, QUEUE, Recorder, add, apply, read, save,
};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

#[test]
fn history_roundtrip_groups_observations_and_bounds_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    let mut h = History {
        schema_version: 1,
        ..History::default()
    };
    for n in 0..(MAX_CYCLES + 4) {
        apply(
            &mut h,
            Message::Begin(json!({"threads": 2, "username": "private-username"})),
        );
        for _ in 0..3 {
            add(&mut h, "exact_lookup_rejection_v1", json!({"rejected_requests": n, "expected_owner": "absent", "stage": "master_reference_scope", "reason": "owner_unqualified"}).as_object().unwrap().clone());
        }
        apply(
            &mut h,
            Message::Complete(
                json!({"bytes_downloaded": 604, "disk_bytes_written": 17}),
                "partial_failure",
            ),
        );
    }
    save(&path, &mut h).unwrap();
    let (reopened, status) = read(&path);
    assert_eq!(status, "available");
    assert_eq!(reopened.cycles.len(), MAX_CYCLES);
    assert_eq!(reopened.cycles_evicted, 4);
    assert_eq!(
        reopened.cycles.last().unwrap().diagnostics[0].observations,
        3
    );
    assert_eq!(
        reopened.cycles.last().unwrap().stats["disk_bytes_written"],
        17
    );
    assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES);
    assert!(
        !serde_json::to_string(&reopened)
            .unwrap()
            .contains("private-username")
    );
}

#[test]
fn interrupted_staging_keeps_previous_snapshot_and_is_bounded_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    let mut h = History {
        schema_version: 1,
        ..History::default()
    };
    apply(&mut h, Message::Begin(json!({"threads": 2})));
    save(&path, &mut h).unwrap();
    std::fs::write(path.with_extension("support-tmp"), br#"{"half-written":1"#).unwrap();
    let (mut reopened, status) = read(&path);
    assert_eq!(status, "available");
    assert!(reopened.cycles[0].completed_at.is_none());
    apply(&mut reopened, Message::Begin(json!({"threads": 4})));
    save(&path, &mut reopened).unwrap();
    assert_eq!(read(&path).0.cycles.len(), 2);
    assert!(!path.with_extension("support-tmp").exists());
}

#[test]
fn groups_and_queue_have_explicit_omission_counts() {
    let mut h = History {
        schema_version: 1,
        ..History::default()
    };
    apply(&mut h, Message::Begin(json!({})));
    for n in 0..(MAX_GROUPS + 5) {
        add(
            &mut h,
            "metadata_capture_ambiguity_counts_v1",
            json!({"matching_children": n}).as_object().unwrap().clone(),
        );
    }
    assert_eq!(h.cycles[0].diagnostics.len(), MAX_GROUPS);
    assert_eq!(h.groups_omitted, 5);
    let (sender, _receiver) = mpsc::sync_channel(QUEUE);
    let recorder = Recorder {
        sender,
        generation: Arc::new(AtomicU64::new(0)),
        dropped: Arc::new(AtomicU64::new(0)),
        aliases: Arc::new(Mutex::new(std::collections::HashMap::new())),
        alias_namespace: uuid::Uuid::new_v4(),
    };
    for _ in 0..(QUEUE + 5) {
        recorder.send(Message::Observe(
            "sparse_identity_state_failed",
            Default::default(),
        ));
    }
    assert_eq!(recorder.dropped.load(Ordering::Relaxed), 5);
}

#[test]
fn diagnostic_layer_runs_below_log_filter_and_ignores_private_fields() {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    let recorder = Recorder {
        sender,
        generation: Arc::new(AtomicU64::new(0)),
        dropped: Arc::new(AtomicU64::new(0)),
        aliases: Arc::new(Mutex::new(std::collections::HashMap::new())),
        alias_namespace: uuid::Uuid::new_v4(),
    };
    let subscriber = tracing_subscriber::registry().with(recorder).with(
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::sink)
            .with_filter(tracing_subscriber::filter::LevelFilter::ERROR),
    );
    tracing::subscriber::with_default(subscriber, || {
        tracing::debug!(
            diagnostic = "exact_lookup_rejection_v1",
            target = "paired",
            stage = "master_reference_scope",
            reason = "owner_unqualified",
            expected_owner = "absent",
            lookup_zone = "primary",
            rejected_requests = 604u64,
            asset_id = "private-id",
            path = "/private/path",
            error = "private-password",
            token = "private-token",
            "private free text"
        );
        tracing::error!(
            diagnostic = "private-new-diagnostic",
            error = "private-body",
            "private message"
        );
    });
    let Message::Tagged(_, message) = receiver.try_recv().unwrap() else {
        panic!("expected attribution envelope")
    };
    let Message::Event(kind, fields) = *message else {
        panic!("expected diagnostic")
    };
    assert_eq!(kind, "exact_lookup_rejection_v1");
    assert_eq!(fields["rejected_requests"], 604);
    assert_eq!(fields.len(), 6);
    assert!(!serde_json::to_string(&fields).unwrap().contains("private"));
    assert!(receiver.try_recv().is_err());
}

#[test]
fn untrusted_saved_strings_and_unknown_schema_never_escape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    let mut h = History {
        schema_version: 1,
        ..History::default()
    };
    apply(
        &mut h,
        Message::Begin(json!({"username": "private-id", "resolution": "https://private-url"})),
    );
    let cycle = h.cycles.last_mut().unwrap();
    cycle.build_version = "0.private-token".into();
    cycle.outcome = "private-error".into();
    cycle.stats = json!({"inventory_drop_library": "private-album", "bytes_downloaded": 1});
    add(&mut h, "exact_lookup_rejection_v1", json!({"reason": "private-error", "target": "private-id", "rejected_requests": 4, "path": "/private/path"}).as_object().unwrap().clone());
    std::fs::write(&path, serde_json::to_vec(&h).unwrap()).unwrap();
    let (clean, status) = read(&path);
    assert_eq!(status, "available");
    assert!(!serde_json::to_string(&clean).unwrap().contains("private"));
    std::fs::write(&path, b"{\"schema_version\":999,\"cycles\":[],\"cycles_evicted\":0,\"queue_dropped\":0,\"groups_omitted\":0,\"previous_history_unavailable\":false}").unwrap();
    assert_eq!(read(&path).1, "unsupported_schema");
    std::fs::write(&path, b"{incomplete").unwrap();
    assert_eq!(read(&path).1, "invalid_or_interrupted");
    std::fs::write(&path, vec![b'x'; usize::try_from(MAX_BYTES + 1).unwrap()]).unwrap();
    assert_eq!(read(&path).1, "truncated_or_unreadable");
}

#[tokio::test]
async fn normal_worker_flushes_restart_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    let (recorder, guard) = super::start(path.clone(), json!({"threads": 2})).unwrap();
    recorder
        .sender
        .send(Message::Event(
            "sparse_deletion_validation_failed".into(),
            Default::default(),
        ))
        .unwrap();
    guard.finish().await;
    let (recorder, guard) = super::start(path.clone(), json!({"threads": 4})).unwrap();
    recorder
        .sender
        .send(Message::Complete(json!({"failed": 1}), "partial_failure"))
        .unwrap();
    guard.finish().await;
    let (history, status) = read(&path);
    assert_eq!(status, "available");
    assert_eq!(history.cycles.len(), 2);
    assert_eq!(history.cycles[1].stats["failed"], 1);
    assert_eq!(
        history.cycles[0].diagnostics[0].kind,
        "sparse_deletion_validation_failed"
    );
}

#[test]
fn serialized_byte_limit_rotates_complete_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    let mut history = History {
        schema_version: 1,
        ..History::default()
    };
    for _ in 0..MAX_CYCLES {
        apply(&mut history, Message::Begin(json!({})));
        // Fill fixed scalar fields with long numeric encodings, not private text.
        for n in 0..MAX_GROUPS {
            let fields = json!({"pages":n,"records":u64::MAX,"transferred_bytes":u64::MAX,"retained_bytes":u64::MAX,
                "page_budget":u64::MAX,"record_budget":u64::MAX,"page_byte_budget":u64::MAX,"retained_byte_budget":u64::MAX,
                "elapsed_secs":1.7976931348623157e308,"reason":"invalid_inventory_evidence","phase":"family_scan","subreason":"child_reference_scope_mismatch",
                "eof_observed":true,"family_context":"unrelated","scope_mismatch_component":"owner","child_soft_deleted":false});
            add(
                &mut history,
                "legacy_inventory_failure_v2",
                fields.as_object().unwrap().clone(),
            );
        }
    }
    assert!(serde_json::to_vec(&history).unwrap().len() as u64 > MAX_BYTES);
    save(&path, &mut history).unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() <= MAX_BYTES);
    assert!(history.cycles_evicted > 0);
    let (reopened, status) = read(&path);
    assert_eq!(status, "available");
    assert_eq!(reopened.cycles_evicted, history.cycles_evicted);
}

#[cfg(unix)]
#[test]
fn staging_symlink_cannot_modify_another_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    let target = dir.path().join("media");
    std::fs::write(&target, b"private media").unwrap();
    std::os::unix::fs::symlink(&target, path.with_extension("support-tmp")).unwrap();
    let mut history = History {
        schema_version: 1,
        ..History::default()
    };
    apply(&mut history, Message::Begin(json!({})));
    assert!(save(&path, &mut history).is_err());
    assert_eq!(std::fs::read(target).unwrap(), b"private media");
}

#[tokio::test]
async fn existing_writer_lock_preserves_prior_history() {
    use fs4::fs_std::FileExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    let mut history = History {
        schema_version: 1,
        ..History::default()
    };
    apply(&mut history, Message::Begin(json!({})));
    save(&path, &mut history).unwrap();
    let before = std::fs::read(&path).unwrap();
    let lock = super::private_options()
        .create(true)
        .truncate(false)
        .open(path.with_extension("support-lock"))
        .unwrap();
    lock.lock_exclusive().unwrap();
    let (_, guard) = super::start(path.clone(), json!({"threads":42})).unwrap();
    guard.finish().await;
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn restarting_does_not_complete_an_interrupted_prior_startup() {
    let mut history = History {
        schema_version: 1,
        ..History::default()
    };
    apply(&mut history, Message::Start(json!({})));
    add(
        &mut history,
        "support_startup_v1",
        json!({"phase":"starting"}).as_object().unwrap().clone(),
    );
    apply(&mut history, Message::Start(json!({})));
    assert!(history.cycles[0].completed_at.is_none());
    assert_eq!(history.cycles[0].outcome, "running");
    add(
        &mut history,
        "support_startup_v1",
        json!({"phase":"starting"}).as_object().unwrap().clone(),
    );
    apply(&mut history, Message::Begin(json!({})));
    assert!(history.cycles[0].completed_at.is_none());
    assert_eq!(history.cycles[1].outcome, "success");
    assert!(history.cycles[1].completed_at.is_some());
}

#[test]
fn dropped_cycle_handoff_cannot_overwrite_prior_completed_evidence() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let recorder = Recorder {
        sender,
        generation: Arc::new(AtomicU64::new(0)),
        dropped: Arc::new(AtomicU64::new(0)),
        aliases: Arc::new(Mutex::new(Default::default())),
        alias_namespace: uuid::Uuid::new_v4(),
    };
    let mut history = History {
        schema_version: 1,
        ..History::default()
    };
    recorder.send(Message::Begin(json!({"threads":2})));
    apply(&mut history, receiver.recv().unwrap());
    recorder.send(Message::Complete(json!({"downloaded":17}), "success"));
    apply(&mut history, receiver.recv().unwrap());
    let prior = serde_json::to_value(&history.cycles[0]).unwrap();
    // A stalled writer has a full observation queue at the next cycle boundary.
    recorder.send(Message::Observe(
        "sparse_identity_state_failed",
        Default::default(),
    ));
    recorder.send(Message::Begin(json!({"threads":4})));
    assert_eq!(recorder.dropped.load(Ordering::Relaxed), 1);
    apply(&mut history, receiver.recv().unwrap());
    recorder.send(Message::Complete(
        json!({"downloaded":604}),
        "partial_failure",
    ));
    apply(&mut history, receiver.recv().unwrap());
    assert_eq!(
        serde_json::to_value(&history.cycles[0]).unwrap()["stats"],
        prior["stats"]
    );
    assert_eq!(
        history.cycles[0].completed_at,
        prior["completed_at"].as_str().map(str::to_owned)
    );
    assert_eq!(history.cycles[0].outcome, "success");
    assert_eq!(history.cycles.len(), 1);
    assert_eq!(history.queue_dropped, 1);
    // A later accepted handoff resumes accurate collection with a separate record.
    recorder.send(Message::Begin(json!({"threads":8})));
    apply(&mut history, receiver.recv().unwrap());
    recorder.send(Message::Complete(json!({"downloaded":2}), "success"));
    apply(&mut history, receiver.recv().unwrap());
    assert_eq!(history.cycles.len(), 2);
    assert_eq!(history.cycles[1].stats["downloaded"], 2);
}

#[cfg(unix)]
#[test]
fn failed_history_rename_never_uses_an_unowned_copy_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("support.json");
    std::fs::create_dir(&path).unwrap();
    let media = dir.path().join("media.jpg");
    std::fs::write(&media, b"private media").unwrap();
    let sibling = path.with_extension(format!("json.kei-xdev-tmp-{}", std::process::id()));
    std::os::unix::fs::symlink(&media, &sibling).unwrap();
    let mut history = History {
        schema_version: 1,
        ..History::default()
    };
    apply(&mut history, Message::Start(json!({})));
    assert!(save(&path, &mut history).is_err());
    assert_eq!(std::fs::read(media).unwrap(), b"private media");
    assert!(
        std::fs::symlink_metadata(sibling)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(path.is_dir());
}
