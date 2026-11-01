//! In-process identities retained across publication and owner settlement.
//!
//! This coordinator does not prove filesystem namespace exclusion and does not
//! coordinate other processes. An abandoned writer leaves its identities held
//! for the rest of this process, including when a blocking publisher outlives
//! the future that spawned it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::fs_util::FileIdentity;

enum Entry {
    Active { id: u64, _pin: Arc<std::fs::File> },
    Uncertain { _pin: Arc<std::fs::File> },
}

static REGISTRY: OnceLock<Mutex<HashMap<FileIdentity, Entry>>> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<HashMap<FileIdentity, Entry>> {
    REGISTRY.get_or_init(Mutex::default)
}

/// An identity reservation. Writers must reserve the output before publishing
/// it; an unfinished writer never releases either identity optimistically.
#[derive(PartialEq, Eq)]
pub(super) struct LeaseBinding {
    id: u64,
    identities: Vec<FileIdentity>,
}

#[must_use]
pub(crate) struct IdentityActivity {
    id: u64,
    identities: Vec<FileIdentity>,
    writer: bool,
    settled: bool,
}

impl IdentityActivity {
    fn begin(file: Arc<std::fs::File>, writer: bool) -> Option<Self> {
        let identity = crate::fs_util::file_identity(&file).ok()?;
        let mut entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.contains_key(&identity) {
            return None;
        }
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()?;
        entries.insert(identity, Entry::Active { id, _pin: file });
        Some(Self {
            id,
            identities: vec![identity],
            writer,
            settled: false,
        })
    }

    pub(super) fn observe(file: Arc<std::fs::File>) -> Option<Self> {
        Self::begin(file, false)
    }

    pub(crate) fn publish(file: Arc<std::fs::File>) -> Option<Self> {
        Self::begin(file, true)
    }

    /// Claim both namespace identities in a single registry operation. The
    /// caller retains and revalidates its own confined input/stage capabilities
    /// before exposing the output. Missing input is valid for create-only work.
    pub(crate) fn begin_writer(
        original: Option<Arc<std::fs::File>>,
        stage: Arc<std::fs::File>,
    ) -> Option<Self> {
        let output_id = crate::fs_util::file_identity(&stage).ok()?;
        let original_id = original
            .as_ref()
            .map(|f| crate::fs_util::file_identity(f))
            .transpose()
            .ok()?;
        if original_id == Some(output_id) {
            return None;
        }
        let mut entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.contains_key(&output_id)
            || original_id.is_some_and(|id| entries.contains_key(&id))
        {
            return None;
        }
        let id = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .ok()?;
        let mut identities = Vec::with_capacity(2);
        if let Some((input_id, input)) = original_id.zip(original) {
            entries.insert(input_id, Entry::Active { id, _pin: input });
            identities.push(input_id);
        }
        entries.insert(output_id, Entry::Active { id, _pin: stage });
        identities.push(output_id);
        Some(Self {
            id,
            identities,
            writer: true,
            settled: false,
        })
    }

    /// Verify that a writer lease still owns precisely the retained stage and
    /// optional original. This is coordination evidence, not publication proof.
    pub(crate) fn validates_writer(
        &self,
        original: Option<&std::fs::File>,
        stage: &std::fs::File,
    ) -> bool {
        if !self.writer || self.settled {
            return false;
        }
        let Ok(output_id) = crate::fs_util::file_identity(stage) else {
            return false;
        };
        let original_id = match original.map(crate::fs_util::file_identity).transpose() {
            Ok(id) => id,
            Err(_) => return false,
        };
        if original_id == Some(output_id)
            || self.identities.len() != 1 + usize::from(original_id.is_some())
            || self.identities.last() != Some(&output_id)
            || original_id.is_some_and(|id| self.identities.first() != Some(&id))
        {
            return false;
        }
        let entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owns = |id| {
            self.identities.contains(&id)
                && matches!(entries.get(&id), Some(Entry::Active { id: active, .. }) if *active == self.id)
        };
        owns(output_id) && original_id.is_none_or(owns)
    }

    /// Atomically claim the staged identity before exposing it at the target.
    pub(crate) fn reserve_output(&mut self, file: Arc<std::fs::File>) -> bool {
        let Ok(identity) = crate::fs_util::file_identity(&file) else {
            return false;
        };
        if !self.writer {
            return false;
        }
        let mut entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Replacing a file with itself is not a two-identity handoff.
        if self.identities.contains(&identity) {
            return false;
        }
        if entries.contains_key(&identity) {
            return false;
        }
        entries.insert(
            identity,
            Entry::Active {
                id: self.id,
                _pin: file,
            },
        );
        self.identities.push(identity);
        true
    }

    /// Verify that this exact live observer still pins the owner's input.
    pub(super) fn validates_observer(&self, file: &std::fs::File) -> bool {
        if self.writer || self.settled || self.identities.len() != 1 {
            return false;
        }
        let Ok(identity) = crate::fs_util::file_identity(file) else {
            return false;
        };
        let Some(expected) = self.identities.first() else {
            return false;
        };
        if identity != *expected {
            return false;
        }
        let entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        matches!(entries.get(&identity), Some(Entry::Active { id, .. }) if *id == self.id)
    }

    #[cfg(test)]
    pub(crate) fn test_settle(&mut self) {
        self.settled = true;
    }

    /// Capture an opaque binding only while every reserved identity is live.
    pub(super) fn commit_binding(&self) -> Option<LeaseBinding> {
        if self.settled || self.identities.is_empty() {
            return None;
        }
        let entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.identities.iter().all(|identity| {
            matches!(entries.get(identity), Some(Entry::Active { id, .. }) if *id == self.id)
        }) {
            return None;
        }
        Some(LeaseBinding {
            id: self.id,
            identities: self.identities.clone(),
        })
    }

    /// A seal is minted only by the owner transaction after its exact commit.
    pub(super) fn settled_with(
        &mut self,
        seal: super::metadata::CommittedLease,
        operation: &str,
    ) -> bool {
        let Some(binding) = self.commit_binding() else {
            return false;
        };
        if !seal.matches(binding, operation) {
            return false;
        }
        self.settled = true;
        true
    }
}

impl Drop for IdentityActivity {
    fn drop(&mut self) {
        let mut entries = registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for identity in &self.identities {
            if matches!(entries.get(identity), Some(Entry::Active { id, .. }) if *id == self.id) {
                if !self.settled {
                    let Some(Entry::Active { _pin, .. }) = entries.remove(identity) else {
                        continue;
                    };
                    entries.insert(*identity, Entry::Uncertain { _pin });
                } else {
                    entries.remove(identity);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IdentityActivity;
    use std::sync::Arc;

    #[test]
    fn output_handoff_blocks_alias_and_preserves_independent_progress() {
        let dir = tempfile::tempdir().unwrap();
        let identity = |name: &str| {
            let file = std::fs::File::create(dir.path().join(name)).unwrap();
            Arc::new(file)
        };
        let original = identity("original");
        let output = identity("staged");
        let independent = identity("independent");
        let mut writer = IdentityActivity::publish(Arc::clone(&original)).unwrap();
        assert!(writer.reserve_output(Arc::clone(&output)));
        // Model the instant after exchange: an alias opening the new target
        // now sees the output identity, while owner settlement is still pending.
        assert!(IdentityActivity::observe(Arc::clone(&output)).is_none());
        assert!(IdentityActivity::observe(Arc::clone(&original)).is_none());
        let mut independent_activity = IdentityActivity::observe(Arc::clone(&independent)).unwrap();
        independent_activity.test_settle();
        drop(independent_activity);
        writer.test_settle();
        drop(writer);
        assert!(IdentityActivity::observe(Arc::clone(&output)).is_some());
    }

    #[test]
    fn poisoned_registry_still_allows_an_independent_file() {
        let poisoned = std::thread::spawn(|| {
            let _guard = super::registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            panic!("synthetic registry owner panic");
        });
        assert!(poisoned.join().is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(std::fs::File::create(dir.path().join("unrelated")).unwrap());
        let mut observer = IdentityActivity::observe(file).unwrap();
        observer.test_settle();
    }

    #[cfg(unix)]
    #[test]
    fn installed_stage_identity_stays_reserved_until_owner_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let stage = dir.path().join("stage");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&stage, b"new").unwrap();
        let input = Arc::new(std::fs::File::open(&target).unwrap());
        let output = Arc::new(std::fs::File::open(&stage).unwrap());
        let mut writer = IdentityActivity::publish(Arc::clone(&input)).unwrap();
        assert!(writer.reserve_output(Arc::clone(&output)));
        // This is a synthetic rename, not kei's production publication path.
        std::fs::rename(&stage, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let installed = Arc::new(std::fs::File::open(&target).unwrap());
        assert!(IdentityActivity::observe(Arc::clone(&installed)).is_none());
        writer.test_settle();
        drop(writer);
        let mut observer = IdentityActivity::observe(installed).unwrap();
        observer.test_settle();
    }

    #[test]
    fn writer_pair_acquisition_is_atomic_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let input = Arc::new(std::fs::File::create(dir.path().join("input-pair")).unwrap());
        let output = Arc::new(std::fs::File::create(dir.path().join("output-pair")).unwrap());
        let mut blocker = IdentityActivity::observe(Arc::clone(&output)).unwrap();
        assert!(
            IdentityActivity::begin_writer(Some(Arc::clone(&input)), Arc::clone(&output)).is_none()
        );
        // A conflict on B must not leave A partially reserved.
        let mut independent_input = IdentityActivity::observe(Arc::clone(&input)).unwrap();
        independent_input.test_settle();
        drop(independent_input);
        blocker.test_settle();
        drop(blocker);
        let mut pair =
            IdentityActivity::begin_writer(Some(Arc::clone(&input)), Arc::clone(&output)).unwrap();
        assert!(pair.validates_writer(Some(&input), &output));
        assert!(!pair.validates_writer(Some(&output), &input));
        assert!(IdentityActivity::observe(input).is_none());
        assert!(IdentityActivity::observe(output).is_none());
        pair.test_settle();
    }

    #[test]
    fn same_inode_is_not_an_output_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(std::fs::File::create(dir.path().join("same")).unwrap());
        let mut writer = IdentityActivity::publish(Arc::clone(&file)).unwrap();
        assert!(!writer.reserve_output(Arc::clone(&file)));
        writer.test_settle();
    }

    #[test]
    fn abandoned_observer_keeps_identity_held() {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(std::fs::File::create(dir.path().join("input")).unwrap());
        let observer = IdentityActivity::observe(Arc::clone(&file)).unwrap();
        drop(observer);
        assert!(IdentityActivity::publish(file).is_none());
    }

    #[test]
    fn cancelled_waiter_keeps_identity_held_while_blocking_publisher_runs() {
        let dir = tempfile::tempdir().unwrap();
        let original = Arc::new(std::fs::File::create(dir.path().join("input")).unwrap());
        let output = Arc::new(std::fs::File::create(dir.path().join("output")).unwrap());
        let unrelated = Arc::new(std::fs::File::create(dir.path().join("other")).unwrap());
        let mut writer = IdentityActivity::publish(Arc::clone(&original)).unwrap();
        assert!(writer.reserve_output(Arc::clone(&output)));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let blocking = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        });
        entered_rx.recv().unwrap();
        // Simulate cancellation of the waiter while the blocking publisher
        // is paused. Its eventual result and owner settlement are unknown.
        drop(writer);
        assert!(IdentityActivity::observe(Arc::clone(&original)).is_none());
        assert!(IdentityActivity::observe(Arc::clone(&output)).is_none());
        let mut unrelated_activity = IdentityActivity::observe(Arc::clone(&unrelated)).unwrap();
        unrelated_activity.test_settle();
        drop(unrelated_activity);
        finish_tx.send(()).unwrap();
        blocking.join().unwrap();
        // Finishing the filesystem callback alone is not owner settlement.
        assert!(IdentityActivity::observe(output).is_none());
    }

    #[test]
    fn abandoned_publisher_keeps_both_identities_held() {
        let dir = tempfile::tempdir().unwrap();
        let original = std::fs::File::create(dir.path().join("a")).unwrap();
        let output = std::fs::File::create(dir.path().join("b")).unwrap();
        let a = Arc::new(original);
        let b = Arc::new(output);
        let mut writer = IdentityActivity::publish(Arc::clone(&a)).unwrap();
        assert!(writer.reserve_output(Arc::clone(&b)));
        // Dropping the caller cannot establish that spawn_blocking stopped.
        drop(writer);
        assert!(IdentityActivity::observe(Arc::clone(&a)).is_none());
        assert!(IdentityActivity::observe(Arc::clone(&b)).is_none());
    }
}
