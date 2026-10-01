//! Journaled Linux replacement when the filesystem cannot exchange entries.
//! Recovery rolls an uncommitted publication back, without overwriting any
//! entry created by another writer. Unknown bytes always retain the journal.

use std::io::{Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    ExistingFileFingerprint,
    replacement::{ConditionalPublishMustRetainPaths, ConditionalPublishTargetChanged},
};
use crate::fs_util::{ConfinedParents, ConfinedPath, FileIdentity, file_identity};

const JOURNAL_PREFIX: &str = ".kei-replace-";
const FORMAT_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 4096;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    size: u64,
    sha256: [u8; 32],
}

impl From<ExistingFileFingerprint> for Fingerprint {
    fn from(value: ExistingFileFingerprint) -> Self {
        Self {
            size: value.size,
            sha256: value.sha256,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    target: Vec<u8>,
    original: Fingerprint,
    replacement: Fingerprint,
}

struct Journal {
    directory: ConfinedPath,
    manifest: ConfinedPath,
    lock: std::fs::File,
    target: ConfinedPath,
    original: ConfinedPath,
    replacement: ConfinedPath,
    rollback: ConfinedPath,
    commit: ConfinedPath,
    evidence: Manifest,
}

impl Drop for Journal {
    fn drop(&mut self) {
        // Explicit unlock also releases a lock held by descriptors inherited
        // between another thread's fork and exec. Closing our descriptor alone
        // can leave that lock temporarily held after this transaction ends.
        if let Err(error) = FileExt::unlock(&self.lock) {
            tracing::warn!(%error, "Could not release replacement journal lock");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Prepared,
    Displaced,
    Installed,
    Committed,
}

fn confined(path: &Path) -> Result<ConfinedPath> {
    Ok(ConfinedPath::open(
        Path::new("/"),
        path,
        ConfinedParents::Existing,
    )?)
}

fn directory_path(target: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .context("Replacement target has no filename")?;
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(name.as_bytes()));
    Ok(target.with_file_name(format!("{JOURNAL_PREFIX}{digest}")))
}

#[cfg(feature = "xmp")]
pub(super) fn cleanup_prepared(
    path: &Path,
    expected: ExistingFileFingerprint,
    expected_identity: FileIdentity,
) -> Result<()> {
    let path = confined(path)?;
    path.validate_identity(expected_identity)?;
    anyhow::ensure!(
        read_fingerprint(&path)? == Some(expected.into()),
        "Prepared sidecar bytes changed; retaining file"
    );
    path.validate_identity(expected_identity)?;
    unlink(&path, 0)?;
    path.sync_parent()?;
    Ok(())
}

fn journal_name(name: &std::ffi::OsStr) -> bool {
    name.as_bytes()
        .strip_prefix(JOURNAL_PREFIX.as_bytes())
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        })
}

fn read_fingerprint(path: &ConfinedPath) -> Result<Option<Fingerprint>> {
    let Some(mut file) = path.open_optional_regular()? else {
        return Ok(None);
    };
    let identity = file_identity(&file)?;
    let mut hash = Sha256::new();
    let size = std::io::copy(&mut file, &mut hash)?;
    path.validate_identity(identity)?;
    Ok(Some(Fingerprint {
        size,
        sha256: hash.finalize().into(),
    }))
}

fn identity(path: &ConfinedPath) -> Result<Option<FileIdentity>> {
    path.open_optional_regular()?
        .map(|file| file_identity(&file))
        .transpose()
        .map_err(Into::into)
}

fn same_entry(left: &ConfinedPath, right: &ConfinedPath) -> Result<bool> {
    let left = identity(left)?;
    Ok(left.is_some() && left == identity(right)?)
}

fn unlink(path: &ConfinedPath, flags: libc::c_int) -> Result<()> {
    // SAFETY: retained directory descriptors and NUL-terminated names outlive
    // the call; unlinkat does not follow the leaf, including directory removal.
    let result = unsafe { libc::unlinkat(path.parent_fd(), path.name_cstr().as_ptr(), flags) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn move_to_owned_slot(source: &ConfinedPath, destination: &ConfinedPath) -> Result<()> {
    let _source = source.open_regular()?;
    anyhow::ensure!(
        destination.open_optional_regular()?.is_none() && !destination.entry_exists()?,
        "Replacement recovery slot already exists: {}",
        destination.path().display()
    );
    // SAFETY: both retained parents and names outlive renameat. The destination
    // is a never-used slot inside the exclusively created, owner-only journal
    // directory. No caller uses this primitive to restore a public target.
    let result = unsafe {
        libc::renameat(
            source.parent_fd(),
            source.name_cstr().as_ptr(),
            destination.parent_fd(),
            destination.name_cstr().as_ptr(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

impl Journal {
    fn paths(
        target: ConfinedPath,
        directory: ConfinedPath,
        lock: std::fs::File,
        evidence: Manifest,
    ) -> Result<Self> {
        let manifest = confined(&directory.path().join("manifest.json"))?;
        Ok(Self {
            original: manifest.sibling(&directory.path().join("original"))?,
            replacement: manifest.sibling(&directory.path().join("replacement"))?,
            rollback: manifest.sibling(&directory.path().join("rollback"))?,
            commit: manifest.sibling(&directory.path().join("committed"))?,
            directory,
            manifest,
            lock,
            target,
            evidence,
        })
    }

    fn create(
        part: &ConfinedPath,
        target: ConfinedPath,
        expected: ExistingFileFingerprint,
        replacement: ExistingFileFingerprint,
    ) -> Result<Self> {
        anyhow::ensure!(
            part.path() != target.path(),
            "Prepared file must differ from the replacement target"
        );
        let directory = target.sibling(&directory_path(target.path())?)?;
        // SAFETY: the retained parent and NUL-terminated name remain live.
        // Exclusive mkdir refuses any existing leaf, including a symlink.
        if unsafe { libc::mkdirat(directory.parent_fd(), directory.name_cstr().as_ptr(), 0o700) }
            != 0
        {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "Replacement journal already exists or cannot be created: {}",
                    directory.path().display()
                )
            });
        }
        let manifest = confined(&directory.path().join("manifest.json"))?;
        let mut lock = manifest.create_new_regular()?;
        anyhow::ensure!(lock.try_lock_exclusive()?, "Replacement journal is busy");
        let evidence = Manifest {
            version: FORMAT_VERSION,
            target: target
                .path()
                .file_name()
                .context("Missing target name")?
                .as_bytes()
                .to_vec(),
            original: expected.into(),
            replacement: replacement.into(),
        };
        lock.write_all(&serde_json::to_vec(&evidence)?)?;
        lock.sync_all()?;
        manifest.sync_parent()?;
        directory.sync_parent()?;
        let journal = Self::paths(target, directory, lock, evidence)?;
        super::platform::hard_link_confined(part, &journal.replacement)?;
        journal.replacement.open_regular()?.sync_all()?;
        journal.replacement.sync_parent()?;
        anyhow::ensure!(
            read_fingerprint(&journal.replacement)?.as_ref() == Some(&journal.evidence.replacement),
            "Prepared replacement changed before displacement"
        );
        Ok(journal)
    }

    fn open(directory: &Path) -> Result<Option<Self>> {
        let directory = confined(directory)?;
        let manifest = confined(&directory.path().join("manifest.json"))?;
        let Some(read) = manifest.open_optional_regular()? else {
            // mkdir may have succeeded immediately before interruption. Only
            // an empty directory can be removed; unknown contents stay intact.
            unlink(&directory, libc::AT_REMOVEDIR)?;
            directory.sync_parent()?;
            return Ok(None);
        };
        let expected_identity = file_identity(&read)?;
        // NFS flock requires a writable descriptor. O_NOFOLLOW rejects leaf
        // links; validate_identity below binds it to the already checked file.
        // SAFETY: the retained parent and name remain valid for openat.
        let raw = unsafe {
            libc::openat(
                manifest.parent_fd(),
                manifest.name_cstr().as_ptr(),
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        use std::os::fd::FromRawFd;
        // SAFETY: openat returned a new descriptor, now owned by this File.
        let mut lock = unsafe { std::fs::File::from_raw_fd(raw) };
        anyhow::ensure!(
            file_identity(&lock)? == expected_identity,
            "Replacement manifest changed while opening"
        );
        anyhow::ensure!(
            lock.try_lock_exclusive()?,
            "Replacement journal is busy: {}",
            directory.path().display()
        );
        manifest.validate_identity(expected_identity)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut lock)
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_MANIFEST_BYTES,
            "Replacement manifest is too large"
        );
        if bytes.is_empty() {
            for entry in std::fs::read_dir(format!("/proc/self/fd/{}", manifest.parent_fd()))? {
                anyhow::ensure!(
                    entry?.file_name() == "manifest.json",
                    "Incomplete manifest has recovery entries; retaining all bytes"
                );
            }
            unlink(&manifest, 0)?;
            unlink(&directory, libc::AT_REMOVEDIR)?;
            directory.sync_parent()?;
            return Ok(None);
        }
        let evidence: Manifest = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            evidence.version == FORMAT_VERSION,
            "Unsupported replacement journal version"
        );
        anyhow::ensure!(
            !evidence.target.is_empty()
                && !evidence.target.contains(&b'/')
                && !evidence.target.contains(&0)
                && evidence.target != b"."
                && evidence.target != b"..",
            "Invalid replacement journal target"
        );
        let name = std::ffi::OsString::from_vec(evidence.target.clone());
        let target_path = directory.path().with_file_name(name);
        anyhow::ensure!(
            directory_path(&target_path)? == directory.path(),
            "Replacement journal target does not match its directory"
        );
        Self::paths(directory.sibling(&target_path)?, directory, lock, evidence).map(Some)
    }

    fn sync(&self) -> Result<()> {
        self.original.sync_parent()?;
        self.target.sync_parent()?;
        Ok(())
    }

    fn committed(&self) -> Result<bool> {
        let Some(file) = self.commit.open_optional_regular()? else {
            return Ok(false);
        };
        let mut bytes = Vec::new();
        file.take(32).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.is_empty() || bytes == b"committed\n",
            "Unknown commit-marker bytes retained"
        );
        Ok(!bytes.is_empty())
    }

    fn finish(&self) -> Result<()> {
        self.committed()?;
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.manifest.parent_fd()))? {
            let name = entry?.file_name();
            anyhow::ensure!(
                [
                    "manifest.json",
                    "original",
                    "replacement",
                    "rollback",
                    "committed"
                ]
                .iter()
                .any(|expected| name == *expected),
                "Unknown entry in replacement journal; retaining all bytes"
            );
        }
        // Delete only byte-verified journal entries. Changed entries, links,
        // unexpected files, or a public-target conflict retain the journal.
        for (path, expected) in [
            (&self.original, &self.evidence.original),
            (&self.replacement, &self.evidence.replacement),
            (&self.rollback, &self.evidence.replacement),
        ] {
            if let Some(actual) = read_fingerprint(path)? {
                anyhow::ensure!(
                    actual == *expected,
                    "Changed replacement recovery bytes retained at {}",
                    path.path().display()
                );
            }
        }
        for path in [&self.original, &self.replacement, &self.rollback] {
            if path.entry_exists()? {
                unlink(path, 0)?;
            }
        }
        if self.commit.entry_exists()? {
            self.commit.open_regular()?;
            unlink(&self.commit, 0)?;
        }
        self.sync()?;
        self.manifest
            .validate_identity(file_identity(&self.lock)?)?;
        unlink(&self.manifest, 0)?;
        unlink(&self.directory, libc::AT_REMOVEDIR)?;
        self.directory.sync_parent()?;
        Ok(())
    }

    fn recover(&self) -> Result<()> {
        if self.committed()? {
            anyhow::ensure!(
                read_fingerprint(&self.target)?.as_ref() == Some(&self.evidence.replacement),
                "Committed replacement target changed; retaining recovery bytes"
            );
            return self.finish();
        }
        if !self.original.entry_exists()? {
            let target = read_fingerprint(&self.target)?;
            anyhow::ensure!(
                target.as_ref() == Some(&self.evidence.original)
                    || target.as_ref() == Some(&self.evidence.replacement),
                "Original target cannot be verified; retaining journal"
            );
            return self.finish();
        }
        if same_entry(&self.target, &self.replacement)?
            && read_fingerprint(&self.target)?.as_ref() == Some(&self.evidence.replacement)
        {
            move_to_owned_slot(&self.target, &self.rollback)?;
            self.sync()?;
            if read_fingerprint(&self.rollback)?.as_ref() != Some(&self.evidence.replacement) {
                super::platform::hard_link_confined(&self.rollback, &self.target)?;
                self.sync()?;
                anyhow::bail!(
                    "Concurrent edit restored during rollback; retaining all recovery bytes"
                );
            }
        }
        if !self.target.entry_exists()? {
            // Unlike rename, hard_link cannot overwrite a concurrent edit.
            super::platform::hard_link_confined(&self.original, &self.target)?;
            self.sync()?;
        }
        anyhow::ensure!(
            same_entry(&self.target, &self.original)?,
            "Concurrent replacement target preserved; original retained at {}",
            self.original.path().display()
        );
        // A changed original is restored but not deleted by fingerprint-based
        // cleanup. Retain evidence rather than treating it as an approved file.
        self.finish()
    }
}

pub(super) fn publish(
    part: &Path,
    target: &Path,
    expected: ExistingFileFingerprint,
    replacement: ExistingFileFingerprint,
) -> Result<()> {
    let journal = Journal::create(&confined(part)?, confined(target)?, expected, replacement)?;
    let result = publish_journal(&journal, |_| Ok(()));
    if let Err(error) = result {
        if let Err(recovery_error) = journal.recover() {
            return Err(retain_recovery_error(
                error.context(format!(
                    "Replacement recovery incomplete: {recovery_error:#}"
                )),
                &journal,
                part,
            ));
        }
        return Err(error);
    }
    finish_publication(&journal, part).map_err(|error| retain_recovery_error(error, &journal, part))
}

fn retain_recovery_error(error: anyhow::Error, journal: &Journal, part: &Path) -> anyhow::Error {
    error
        .context(ConditionalPublishMustRetainPaths {
            paths: vec![part.to_path_buf(), journal.directory.path().to_path_buf()],
        })
        .context(ConditionalPublishTargetChanged::Unverifiable {
            path: journal.target.path().to_path_buf(),
        })
}

fn finish_publication(journal: &Journal, part: &Path) -> Result<()> {
    journal.finish()?;
    let part = confined(part)?;
    if same_entry(&part, &journal.target)?
        && read_fingerprint(&part)?.as_ref() == Some(&journal.evidence.replacement)
    {
        unlink(&part, 0)?;
        part.sync_parent()?;
    }
    Ok(())
}

fn publish_journal(journal: &Journal, mut hook: impl FnMut(Stage) -> Result<()>) -> Result<()> {
    hook(Stage::Prepared)?;
    move_to_owned_slot(&journal.target, &journal.original)?;
    journal.sync()?;
    hook(Stage::Displaced)?;
    if read_fingerprint(&journal.original)?.as_ref() != Some(&journal.evidence.original) {
        return Err(ConditionalPublishTargetChanged::AfterPlanning {
            path: journal.target.path().to_path_buf(),
        }
        .into());
    }
    anyhow::ensure!(
        read_fingerprint(&journal.replacement)?.as_ref() == Some(&journal.evidence.replacement),
        "Prepared replacement changed after displacement"
    );
    super::platform::hard_link_confined(&journal.replacement, &journal.target)?;
    journal.sync()?;
    hook(Stage::Installed)?;
    anyhow::ensure!(
        read_fingerprint(&journal.target)?.as_ref() == Some(&journal.evidence.replacement)
            && read_fingerprint(&journal.original)?.as_ref() == Some(&journal.evidence.original),
        "Replacement bytes changed during publication"
    );
    let mut commit = journal.commit.create_new_regular()?;
    commit.write_all(b"committed\n")?;
    commit.sync_all()?;
    journal.sync()?;
    hook(Stage::Committed)?;
    Ok(())
}

pub(super) fn recover_target(target: &Path) -> Result<()> {
    let directory = directory_path(target)?;
    if std::fs::symlink_metadata(&directory)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(());
    }
    if let Some(journal) = Journal::open(&directory)? {
        journal.recover()?;
    }
    Ok(())
}

/// Recover journals before discovery or checksum checks can misclassify an
/// interrupted displacement as a missing or changed media file. The configured
/// root is resolved first; descendant links are not traversed, and mutations
/// use retained, no-follow directory capabilities.
pub(super) async fn recover_tree(root: &Path) -> Result<()> {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || recover_tree_blocking(&root)).await??;
    Ok(())
}

fn recover_tree_blocking(root: &Path) -> Result<()> {
    if !root.try_exists()? {
        return Ok(());
    }
    anyhow::ensure!(
        !root
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
        "Replacement recovery root contains a parent component"
    );
    // The configured root is a trusted anchor and may be an alias. Descendants
    // and journals still undergo no-follow traversal from its resolved path.
    let mut directories = vec![std::fs::canonicalize(root)?];
    while let Some(directory) = directories.pop() {
        let capability = confined(&directory.join(".kei-recovery-probe"))?;
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", capability.parent_fd()))? {
            let entry = entry?;
            if journal_name(&entry.file_name()) {
                if let Some(journal) = Journal::open(&directory.join(entry.file_name()))
                    .with_context(|| {
                        format!(
                            "Could not open replacement journal {}",
                            directory.join(entry.file_name()).display()
                        )
                    })?
                {
                    journal.recover().with_context(|| format!("Could not recover replacement journal {}; all ambiguous bytes are retained", directory.join(entry.file_name()).display()))?;
                }
            } else if entry.file_type()?.is_dir() {
                directories.push(directory.join(entry.file_name()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
