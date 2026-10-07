//! Disposable-path observation of an actual failed initial receipt transaction.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
struct Probe {
    observed: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
}
type Probes = std::collections::HashMap<PathBuf, Arc<Probe>>;
fn probes() -> &'static Mutex<Probes> {
    static PROBES: OnceLock<Mutex<Probes>> = OnceLock::new();
    PROBES.get_or_init(|| Mutex::new(Probes::new()))
}

pub(in crate::download) struct FailedFinalizationProbe {
    path: PathBuf,
    probe: Arc<Probe>,
}

impl FailedFinalizationProbe {
    pub(in crate::download) fn for_path(path: &Path) -> Self {
        let probe = Arc::new(Probe::default());
        assert!(
            probes()
                .lock()
                .unwrap()
                .insert(path.into(), probe.clone())
                .is_none()
        );
        Self {
            path: path.into(),
            probe,
        }
    }

    pub(in crate::download) async fn observed(&self) {
        self.probe.observed.notified().await;
    }

    pub(in crate::download) fn release(&self) {
        self.probe.proceed.notify_one();
    }
}

impl Drop for FailedFinalizationProbe {
    fn drop(&mut self) {
        self.release();
        probes().lock().unwrap().remove(&self.path);
    }
}

pub(super) async fn observe(path: &Path) {
    let probe = probes().lock().unwrap().get(path).cloned();
    if let Some(probe) = probe {
        probe.observed.notify_one();
        probe.proceed.notified().await;
    }
}
