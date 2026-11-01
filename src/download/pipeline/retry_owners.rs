//! Invocation-local selection owners for failed tasks.
//! These values are not source attestations or publication authority.
use crate::download::{
    RetryTaskKey, filter::DownloadTask, orchestration::generation::SelectionRun,
};
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// The producing invocation issues this with each queued task.
#[derive(Clone)]
pub(super) enum ProducerOrigin {
    Selected {
        run: Arc<SelectionRun>,
        pass: String,
    },
    Unknown,
}

pub(super) struct QueuedTask {
    pub(super) task: DownloadTask,
    pub(super) origin: ProducerOrigin,
}

impl From<DownloadTask> for QueuedTask {
    fn from(task: DownloadTask) -> Self {
        Self {
            task,
            origin: ProducerOrigin::Unknown,
        }
    }
}

#[derive(Clone)]
struct Owner {
    run: Arc<SelectionRun>,
    pass: String,
    original: DownloadTask,
}

#[derive(Clone)]
enum Entry {
    Unique(Owner),
    Ordinary(DownloadTask),
    Ambiguous,
}

pub(in crate::download) enum ResolvedOwner<'a> {
    Selected(&'a Arc<SelectionRun>, &'a str),
    Ordinary,
}

#[derive(Clone, Default)]
pub(in crate::download) struct RetryOwners {
    entries: FxHashMap<RetryTaskKey, Entry>,
    saw_selection: bool,
}

impl std::fmt::Debug for RetryOwners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryOwners")
            .field("entries", &self.entries.len())
            .finish()
    }
}

fn same_task(a: &DownloadTask, b: &DownloadTask) -> bool {
    a.require_created_entry == b.require_created_entry
        && a.replacement_fingerprint == b.replacement_fingerprint
        && a.pending_cross_parent_root == b.pending_cross_parent_root
        && Arc::ptr_eq(&a.metadata, &b.metadata)
        && a.created_local == b.created_local
        && a.created_local.offset().local_minus_utc() == b.created_local.offset().local_minus_utc()
        && a.media_type == b.media_type
        && a.library == b.library
        && a.asset_id == b.asset_id
        && a.asset_record_name == b.asset_record_name
        && a.version_size == b.version_size
        && a.download_path == b.download_path
        && a.checksum == b.checksum
        && a.size == b.size
        && a.publication_binding == b.publication_binding
}

impl RetryOwners {
    pub(super) fn record(&mut self, task: &DownloadTask, origin: ProducerOrigin) {
        let key = RetryTaskKey::from(task);
        match origin {
            ProducerOrigin::Selected { run, pass } => {
                self.saw_selection = true;
                if task
                    .publication_binding
                    .as_deref()
                    .is_some_and(|binding| binding.origin_pass == pass)
                {
                    self.insert(
                        key,
                        Owner {
                            run,
                            pass,
                            original: task.clone(),
                        },
                    );
                } else {
                    self.entries.insert(key, Entry::Ambiguous);
                }
            }
            // This preserves the existing ordinary path. It is not evidence
            // that no retained selected owner overlaps the same resource.
            ProducerOrigin::Unknown => self.insert_ordinary(key, task.clone()),
        }
    }

    fn insert_ordinary(&mut self, key: RetryTaskKey, task: DownloadTask) {
        use std::collections::hash_map::Entry as MapEntry;
        match self.entries.entry(key) {
            MapEntry::Vacant(entry) => {
                entry.insert(Entry::Ordinary(task));
            }
            MapEntry::Occupied(mut entry) => {
                let identical = matches!(entry.get(), Entry::Ordinary(old) if same_task(old, &task) && old.url == task.url);
                if !identical {
                    entry.insert(Entry::Ambiguous);
                }
            }
        }
    }

    fn insert(&mut self, key: RetryTaskKey, owner: Owner) {
        use std::collections::hash_map::Entry as MapEntry;
        match self.entries.entry(key) {
            MapEntry::Vacant(entry) => {
                entry.insert(Entry::Unique(owner));
            }
            MapEntry::Occupied(mut entry) => {
                let identical = match entry.get() {
                    Entry::Unique(old) => {
                        Arc::ptr_eq(&old.run, &owner.run)
                            && old.pass == owner.pass
                            && same_task(&old.original, &owner.original)
                            && old.original.url == owner.original.url
                    }
                    Entry::Ordinary(_) | Entry::Ambiguous => false,
                };
                if !identical {
                    entry.insert(Entry::Ambiguous);
                }
            }
        }
    }

    pub(in crate::download) fn merge(&mut self, other: Self) {
        self.saw_selection |= other.saw_selection;
        for (key, entry) in other.entries {
            match entry {
                Entry::Unique(owner) => self.insert(key, owner),
                Entry::Ordinary(task) => self.insert_ordinary(key, task),
                Entry::Ambiguous => {
                    self.entries.insert(key, Entry::Ambiguous);
                }
            }
        }
    }

    pub(super) fn saw_selection(&self) -> bool {
        self.saw_selection
    }

    pub(in crate::download) fn get(&self, task: &DownloadTask) -> Option<ResolvedOwner<'_>> {
        match self.entries.get(&RetryTaskKey::from(task))? {
            Entry::Unique(owner) if same_task(&owner.original, task) => {
                Some(ResolvedOwner::Selected(&owner.run, &owner.pass))
            }
            Entry::Ordinary(original) if same_task(original, task) => Some(ResolvedOwner::Ordinary),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, ProducerOrigin, ResolvedOwner, RetryOwners, RetryTaskKey, same_task};
    use crate::download::filter::{DownloadTask, MetadataPayload};
    use crate::state::{MediaType, VersionSizeKey};
    use std::sync::Arc;

    fn task() -> DownloadTask {
        DownloadTask {
            require_created_entry: true,
            url: "https://synthetic.invalid/old".into(),
            download_path: "/tmp/owned-component".into(),
            replacement_fingerprint: None,
            pending_cross_parent_root: None,
            publication_binding: Some(Arc::new(crate::state::db::PublicationBinding {
                account_key: "account".into(),
                provider_key: "provider".into(),
                scope: "scope".into(),
                library: "zone".into(),
                child: "child".into(),
                master: "master".into(),
                state_id: "state".into(),
                origin_pass: "pass".into(),
            })),
            checksum: "checksum".into(),
            asset_id: Arc::from("state"),
            asset_record_name: Arc::from("child"),
            library: Arc::from("zone"),
            metadata: Arc::new(MetadataPayload::default()),
            size: 5,
            created_local: chrono::Utc::now().fixed_offset(),
            version_size: VersionSizeKey::Original,
            media_type: MediaType::Photo,
        }
    }

    #[test]
    fn refreshed_url_preserves_identity_but_other_owner_facts_do_not() {
        let original = task();
        let mut refreshed = original.clone();
        refreshed.url = "https://synthetic.invalid/new".into();
        assert!(same_task(&original, &refreshed));
        refreshed.asset_record_name = Arc::from("other-child");
        assert!(!same_task(&original, &refreshed));
        refreshed = original.clone();
        Arc::make_mut(refreshed.publication_binding.as_mut().unwrap()).origin_pass =
            "other-pass".into();
        assert!(!same_task(&original, &refreshed));
        refreshed = original.clone();
        refreshed.checksum = "other".into();
        assert!(!same_task(&original, &refreshed));
        refreshed = original.clone();
        refreshed.metadata = Arc::new(MetadataPayload::default());
        assert!(!same_task(&original, &refreshed));
        refreshed = original.clone();
        refreshed.require_created_entry = false;
        assert!(!same_task(&original, &refreshed));
        refreshed = original.clone();
        let shifted = if original.created_local.offset().local_minus_utc() == 3600 {
            7200
        } else {
            3600
        };
        refreshed.created_local = original
            .created_local
            .with_timezone(&chrono::FixedOffset::east_opt(shifted).unwrap());
        assert!(!same_task(&original, &refreshed));
    }

    #[test]
    fn unknown_producer_origin_keeps_ordinary_path_without_claiming_selection() {
        let original = task();
        let mut owners = RetryOwners::default();
        owners.record(&original, ProducerOrigin::Unknown);
        assert!(!owners.saw_selection());
        assert!(matches!(
            owners.get(&original),
            Some(ResolvedOwner::Ordinary)
        ));
    }

    #[test]
    fn ambiguous_key_cannot_be_downgraded_to_ordinary() {
        let original = task();
        let key = RetryTaskKey::from(&original);
        let mut owners = RetryOwners::default();
        owners.entries.insert(key, Entry::Ambiguous);
        owners.record(&original, ProducerOrigin::Unknown);
        assert!(owners.get(&original).is_none());
    }

    #[test]
    fn ambiguous_and_missing_owner_never_resolve_during_merge() {
        let task = task();
        let key = RetryTaskKey::from(&task);
        let mut a = RetryOwners::default();
        assert!(a.get(&task).is_none());
        let mut b = RetryOwners::default();
        b.entries.insert(key.clone(), Entry::Ambiguous);
        a.merge(b);
        assert!(a.get(&task).is_none());
        let mut c = RetryOwners::default();
        c.entries.insert(key, Entry::Ambiguous);
        a.merge(c);
        assert!(a.get(&task).is_none());
    }
}
