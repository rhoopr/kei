//! Stable staging identity and destination lifetime coordination.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::download::error::DownloadError;

fn destination_digest(path: &Path) -> anyhow::Result<Sha256> {
    let path = crate::fs_util::absolute_confined_path(path)?;
    let mut hash = Sha256::new();
    hash.update(b"kei-download-destination-v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        #[cfg(target_os = "macos")]
        hash.update(bytes.to_ascii_lowercase());
        #[cfg(not(target_os = "macos"))]
        hash.update(bytes);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            // Match the filesystem owner's ASCII case normalization without
            // converting through a lossy Unicode string.
            let unit = if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
                unit + u16::from(b'a' - b'A')
            } else {
                unit
            };
            hash.update(unit.to_le_bytes());
        }
    }
    Ok(hash)
}

pub(super) fn staging_path(path: &Path, checksum: &[u8], suffix: &str) -> anyhow::Result<PathBuf> {
    let destination = data_encoding::BASE32_NOPAD.encode(&destination_digest(path)?.finalize());
    let mut generation = Sha256::new();
    generation.update(b"kei-provider-generation-v1\0");
    generation.update(checksum);
    let generation = data_encoding::BASE32_NOPAD.encode(&generation.finalize());
    Ok(path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("kei-v1-{destination}-{generation}{suffix}")))
}

type DestinationKey = [u8; 32];
type DestinationMutex = tokio::sync::Mutex<()>;
type Registry = Mutex<HashMap<DestinationKey, Weak<DestinationMutex>>>;
static DESTINATIONS: OnceLock<Registry> = OnceLock::new();

fn mutex_for_key(key: DestinationKey) -> anyhow::Result<Arc<DestinationMutex>> {
    let mut registry = DESTINATIONS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("Staging coordinator was poisoned"))?;
    if let Some(mutex) = registry.get(&key).and_then(Weak::upgrade) {
        return Ok(mutex);
    }
    let mutex = Arc::new(DestinationMutex::new(()));
    registry.insert(key, Arc::downgrade(&mutex));
    Ok(mutex)
}

fn part_destination_key(path: &Path) -> Option<DestinationKey> {
    let stem = path.file_name()?.to_str()?.strip_prefix("kei-v1-")?;
    let (destination, generation) = stem.split_once('-')?;
    // Versioned names contain fixed-length, lossless digests. A legacy name
    // never grants a destination coordination key.
    let generation = generation.get(..52)?;
    let valid = |value: &str| {
        value.len() == 52
            && value
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || (b'2'..=b'7').contains(&byte))
    };
    if !valid(destination) || !valid(generation) {
        return None;
    }
    data_encoding::BASE32_NOPAD
        .decode(destination.as_bytes())
        .ok()?
        .try_into()
        .ok()
}

/// Owned guards and waiting lock futures retain the registry's strong owner.
/// Lookup, weak upgrade, insertion and pruning share one short synchronous
/// critical section, so cancellation cannot split a live destination mutex.
#[derive(Debug)]
pub(in crate::download) struct StagingGuard {
    key: DestinationKey,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        // Release the strong owner first, then prune under the registry lock
        // used by acquisition. A newly acquired owner must remain intact.
        drop(self.guard.take());
        if let Some(registry) = DESTINATIONS.get()
            && let Ok(mut registry) = registry.lock()
            && registry
                .get(&self.key)
                .is_some_and(|mutex| mutex.strong_count() == 0)
        {
            registry.remove(&self.key);
        }
    }
}

/// Serialize one destination across all generations and suffixes inside this
/// process. Retain from before claim/resume through metadata, publication and
/// retirement. This is not an interprocess or network filesystem lease.
pub(in crate::download) async fn lock_download_destination(
    destination: &Path,
    shutdown: &CancellationToken,
) -> Result<StagingGuard, DownloadError> {
    let key = destination_digest(destination)
        .map_err(DownloadError::Other)?
        .finalize()
        .into();
    // Create the pruning guard before constructing the waiting future. The
    // future drops its Arc before this guard cleans up on cancellation/abort.
    let mut guard = StagingGuard { key, guard: None };
    let mutex = mutex_for_key(key).map_err(DownloadError::Other)?;
    guard.guard = Some(tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            return Err(DownloadError::Interrupted {
                path: destination.display().to_string().into(), bytes_written: 0,
            });
        }
        guard = mutex.lock_owned() => guard,
    });
    Ok(guard)
}

/// Cleanup holds one guard per destination through durable retirement, reusing
/// it for multiple stale generations. Legacy names retain the exact-path
/// ownership policy. A busy current worker preserves both bytes and its claim.
#[derive(Debug, Default)]
pub(in crate::download) struct CleanupLeases {
    held: HashMap<DestinationKey, StagingGuard>,
}

impl CleanupLeases {
    pub(in crate::download) fn try_acquire(&mut self, path: &Path) -> anyhow::Result<bool> {
        let Some(key) = part_destination_key(path) else {
            return Ok(true);
        };
        if self.held.contains_key(&key) {
            return Ok(true);
        }
        let mut guard = StagingGuard { key, guard: None };
        let mutex = mutex_for_key(key)?;
        let Ok(acquired) = mutex.try_lock_owned() else {
            return Ok(false);
        };
        guard.guard = Some(acquired);
        self.held.insert(key, guard);
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
