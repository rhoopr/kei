//! Exclusive embedded temporary files and stable-input publication.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

static EMBED_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn publish_prepared_embed(
    temp_path: &Path,
    final_path: &Path,
    expected: crate::download::file::ExistingFileFingerprint,
    expected_replacement: crate::download::file::ExistingFileFingerprint,
) -> anyhow::Result<()> {
    // CONTRACT: METADATA_EMBED_REWRITE_REQUIRES_STABLE_INPUT
    crate::download::file::publish_file_if_unchanged_blocking(
        temp_path,
        final_path,
        expected,
        expected_replacement,
    )
}

pub(super) fn fingerprint_bytes(
    bytes: &[u8],
) -> Result<crate::download::file::ExistingFileFingerprint> {
    use sha2::{Digest, Sha256};

    Ok(crate::download::file::ExistingFileFingerprint {
        size: u64::try_from(bytes.len()).context("Metadata source is too large to fingerprint")?,
        sha256: Sha256::digest(bytes).into(),
    })
}

fn embed_temp_path(path: &Path, temp_suffix: &str, sequence: u64) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".kei-metadata-{}-{sequence}{temp_suffix}",
        std::process::id()
    ))
}

pub(super) fn create_unique_embed_temp(
    path: &Path,
    temp_suffix: &str,
) -> Result<(std::fs::File, PathBuf)> {
    create_unique_embed_temp_with_sequence(path, temp_suffix, || {
        EMBED_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    })
}

fn create_unique_embed_temp_with_sequence(
    path: &Path,
    temp_suffix: &str,
    mut next_sequence: impl FnMut() -> u64,
) -> Result<(std::fs::File, PathBuf)> {
    loop {
        let candidate = embed_temp_path(path, temp_suffix, next_sequence());
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not create temporary embedded metadata file {}",
                        candidate.display()
                    )
                });
            }
        }
    }
}

/// Remove the tmp file on drop unless disarmed. Protects metadata temp files
/// against panics or writer failures; no orphan sweep matches this suffix.
#[derive(Debug)]
pub(super) struct TmpGuard {
    path: PathBuf,
    armed: bool,
    cleanup_permissions: Option<std::fs::Permissions>,
}

impl TmpGuard {
    #[cfg(any(test, feature = "xmp"))]
    pub(super) fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: true,
            cleanup_permissions: None,
        }
    }

    pub(super) fn with_cleanup_permissions(path: &Path, permissions: std::fs::Permissions) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: true,
            cleanup_permissions: Some(permissions),
        }
    }

    pub(super) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TmpGuard {
    fn drop(&mut self) {
        if self.armed {
            if self.path.exists()
                && let Some(permissions) = self.cleanup_permissions.take()
                && let Err(error) = std::fs::set_permissions(&self.path, permissions)
            {
                tracing::warn!(target: "kei::download::metadata",
                    path = %self.path.display(),
                    %error,
                    "Could not restore temporary metadata permissions for cleanup; retaining file"
                );
                return;
            }
            crate::fs_util::log_remove(&self.path);
        }
    }
}

pub(in crate::download) struct PreparedMetadataFile {
    pub(super) guard: TmpGuard,
    pub(super) expected_input: crate::download::file::ExistingFileFingerprint,
    pub(super) expected_output: crate::download::file::ExistingFileFingerprint,
}

impl PreparedMetadataFile {
    pub(in crate::download) fn output_fingerprint(
        &self,
    ) -> crate::download::file::ExistingFileFingerprint {
        self.expected_output
    }

    pub(in crate::download) fn publish(
        self,
        final_path: &Path,
    ) -> Result<crate::download::file::ExistingFileFingerprint> {
        self.publish_with(final_path, publish_prepared_embed)
    }

    pub(super) fn publish_with(
        self,
        final_path: &Path,
        install: impl FnOnce(
            &Path,
            &Path,
            crate::download::file::ExistingFileFingerprint,
            crate::download::file::ExistingFileFingerprint,
        ) -> anyhow::Result<()>,
    ) -> Result<crate::download::file::ExistingFileFingerprint> {
        let Self {
            guard,
            expected_input,
            expected_output,
        } = self;
        let temp_path = guard.path.clone();
        if let Err(error) = install(&temp_path, final_path, expected_input, expected_output) {
            let disposition = crate::download::file::classify_conditional_publish_error(&error);
            if disposition
                .retained_paths
                .iter()
                .any(|retained| retained == &temp_path)
            {
                guard.disarm();
            }
            return Err(error).with_context(|| {
                format!(
                    "Could not install metadata update {} -> {}",
                    temp_path.display(),
                    final_path.display()
                )
            });
        }
        guard.disarm();
        Ok(expected_output)
    }
}

#[cfg(all(test, feature = "xmp"))]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    #[cfg(feature = "xmp")]
    use super::super::test_support::test_tmp_dir;
    use super::{TmpGuard, create_unique_embed_temp_with_sequence, embed_temp_path};
    use std::fs;

    #[test]
    fn tmp_guard_cleans_up_on_drop() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("armed.meta-tmp");
        fs::write(&path, b"pending").unwrap();
        {
            let _guard = TmpGuard::new(&path);
            assert!(path.exists(), "precondition: tmp file exists");
        }
        assert!(
            !path.exists(),
            "TmpGuard Drop must remove the tmp file on scope exit"
        );
    }

    #[test]
    fn tmp_guard_disarm_keeps_file() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disarmed.meta-tmp");
        fs::write(&path, b"keep me").unwrap();
        {
            let guard = TmpGuard::new(&path);
            guard.disarm();
        }
        assert!(path.exists(), "disarmed TmpGuard must not delete the file");
        fs::remove_file(&path).ok();
    }

    /// The xmp_toolkit writer runs closures across an FFI boundary, so a
    /// panic out of that FFI must still clean up `.meta-tmp`.
    #[test]
    fn tmp_guard_cleans_up_even_on_panic() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("panic.meta-tmp");
        fs::write(&path, b"about to panic").unwrap();

        let path_for_closure = path.clone();
        let joined = std::panic::catch_unwind(move || {
            let _guard = TmpGuard::new(&path_for_closure);
            panic!("simulated xmp_toolkit FFI panic");
        });
        assert!(joined.is_err(), "closure was expected to panic");
        assert!(
            !path.exists(),
            "tmp file must be removed even when the work panics"
        );
    }

    #[test]
    fn create_unique_embed_temp_preserves_regular_file_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        let collision = embed_temp_path(&path, ".meta-tmp", 31);
        let unique = embed_temp_path(&path, ".meta-tmp", 32);
        fs::write(&collision, b"unrelated bytes").unwrap();
        let mut sequences = [31, 32].into_iter();

        let (file, created) = create_unique_embed_temp_with_sequence(&path, ".meta-tmp", || {
            sequences.next().unwrap()
        })
        .unwrap();
        drop(file);

        assert_eq!(created, unique);
        assert_eq!(fs::read(&collision).unwrap(), b"unrelated bytes");
        fs::remove_file(created).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn create_unique_embed_temp_preserves_symlink_collision() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.heic");
        let target = dir.path().join("unrelated");
        let collision = embed_temp_path(&path, ".meta-tmp", 51);
        let unique = embed_temp_path(&path, ".meta-tmp", 52);
        fs::write(&target, b"unrelated bytes").unwrap();
        symlink(&target, &collision).unwrap();
        let mut sequences = [51, 52].into_iter();

        let (file, created) = create_unique_embed_temp_with_sequence(&path, ".meta-tmp", || {
            sequences.next().unwrap()
        })
        .unwrap();
        drop(file);

        assert_eq!(created, unique);
        assert_eq!(fs::read_link(&collision).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"unrelated bytes");
        fs::remove_file(created).unwrap();
    }
}

#[cfg(all(test, not(feature = "xmp")))]
mod native_tests {
    use super::TmpGuard;
    use std::fs;

    #[test]
    fn native_tmp_guard_cleans_configured_temp_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guarded.kei-tmp");
        fs::write(&path, b"pending").unwrap();
        {
            let _guard = TmpGuard::new(&path);
            assert!(path.exists(), "precondition: tmp file exists");
        }
        assert!(
            !path.exists(),
            "TmpGuard Drop must remove configured metadata temp files"
        );
    }
}

// Probe and preparation execute in blocking tasks. One process-wide lane bounds
// control-data/parser allocations independently of up to 64 download workers.
// No media-sized allocations remain; oversized metadata is refused before read.
#[cfg(feature = "xmp")]
static HEIF_METADATA_ADMISSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(feature = "xmp")]
pub(super) fn admit_heif_metadata() -> HeifMetadataAdmission {
    // This lock protects only admission, not shared parser state. A panicking
    // writer drops its owned buffers/temp; poison must not disable future work.
    let guard = HEIF_METADATA_ADMISSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(test)]
    {
        let active = HEIF_ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        HEIF_PEAK.fetch_max(active, std::sync::atomic::Ordering::SeqCst);
    }
    HeifMetadataAdmission { _guard: guard }
}

#[cfg(feature = "xmp")]
#[must_use]
pub(super) struct HeifMetadataAdmission {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, feature = "xmp"))]
static HEIF_ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(test, feature = "xmp"))]
static HEIF_PEAK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, feature = "xmp"))]
impl Drop for HeifMetadataAdmission {
    fn drop(&mut self) {
        HEIF_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) fn fingerprint_open_file(
    file: &mut std::fs::File,
    path: &Path,
) -> anyhow::Result<crate::download::file::ExistingFileFingerprint> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    Ok(crate::download::file::fingerprint_open_file_snapshot_blocking(file, path)?.fingerprint)
}

#[cfg(all(test, feature = "xmp"))]
mod heif_admission_tests {
    use super::{HEIF_PEAK, admit_heif_metadata};
    use crate::download::metadata::MetadataWrite;
    use crate::download::metadata::heif_writer::prepare_metadata_heif;
    use crate::download::metadata::probe::probe_exif;
    use crate::download::metadata::test_support::embed_temp_entries;
    use std::sync::atomic::Ordering;

    #[test]
    fn heif_admission_bounds_actual_probe_and_prepare_at_download_concurrency() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("synthetic.heic");
        // A generated file, with no live account or provider access.
        let bytes = crate::download::heif::apple_multi_exif_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
        );
        std::fs::write(&source, bytes).unwrap();
        let missing = directory.path().join("missing.heic");
        for concurrency in [10, 64] {
            let barrier = std::sync::Barrier::new(concurrency);
            std::thread::scope(|scope| {
                for _ in 0..concurrency {
                    let barrier = &barrier;
                    let source = &source;
                    let missing = &missing;
                    scope.spawn(move || {
                        barrier.wait();
                        let _ = probe_exif(source).unwrap();
                        assert!(
                            prepare_metadata_heif(
                                missing,
                                &MetadataWrite::default(),
                                ".meta-tmp",
                                None
                            )
                            .is_err()
                        );
                        let prepared = prepare_metadata_heif(
                            source,
                            &MetadataWrite {
                                rating: Some(4),
                                ..MetadataWrite::default()
                            },
                            ".meta-tmp",
                            None,
                        )
                        .unwrap();
                        // Cancellation after preparation owns and cleans its temp.
                        drop(prepared);
                    });
                }
            });
            assert!(embed_temp_entries(directory.path(), ".meta-tmp").is_empty());
            assert_eq!(HEIF_PEAK.load(Ordering::SeqCst), 1);
        }
        // The same lane remains usable after all failure and dropped-output paths.
        assert!(probe_exif(&source).is_ok());
    }

    #[tokio::test]
    async fn cancelled_blocking_waiter_finishes_and_releases_admission() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("synthetic.heic");
        let bytes = crate::download::heif::apple_multi_exif_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
        );
        std::fs::write(&source, bytes).unwrap();
        let lane = admit_heif_metadata();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let worker_source = source.clone();
        let worker = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            let result = prepare_metadata_heif(
                &worker_source,
                &MetadataWrite {
                    rating: Some(4),
                    ..MetadataWrite::default()
                },
                ".meta-tmp",
                None,
            );
            let succeeded = result.is_ok();
            drop(result);
            let _ = finished_tx.send(succeeded);
        });
        started_rx.recv().unwrap();
        // Tokio cannot stop an executing blocking closure. Its result is still
        // dropped, and the admission/temporary-file guards must be released.
        worker.abort();
        drop(lane);
        assert!(finished_rx.await.unwrap());
        assert!(embed_temp_entries(directory.path(), ".meta-tmp").is_empty());
        assert!(probe_exif(&source).is_ok());
    }
}
