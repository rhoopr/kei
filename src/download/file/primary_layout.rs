//! Confined, preservation-backed media handover and alias retirement.

#[cfg(any(windows, target_os = "linux"))]
use super::fingerprint::ExistingFileFingerprint;
use super::fingerprint::fingerprint_open_file_snapshot_blocking;
use crate::fs_util::{ConfinedParents, ConfinedPath};
use crate::state::db::primary_layout::LayoutFingerprint;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub(in crate::download) async fn snapshot(
    root: &Path,
    path: &Path,
) -> Result<Option<LayoutFingerprint>> {
    let root = super::reconciliation::reconciliation_source_root(root, path)?;
    let (root, path) = (root, path.to_owned());
    tokio::task::spawn_blocking(move || snapshot_blocking(&root, &path)).await?
}

pub(in crate::download) async fn keep_modified(
    root: &Path,
    path: &Path,
    modified: std::time::SystemTime,
) -> Result<()> {
    set_modified(root, path, modified, None, true).await
}

/// A provider time need not round-trip on every filesystem. Keep the nearest
/// representable value and let the publication-time fallback in `publish`
/// make a truncated value observable.
pub(in crate::download) async fn keep_provider_modified(
    root: &Path,
    path: &Path,
    modified: std::time::SystemTime,
    expected: crate::fs_util::FileIdentity,
) -> Result<()> {
    set_modified(root, path, modified, Some(expected), false).await
}

async fn set_modified(
    root: &Path,
    path: &Path,
    modified: std::time::SystemTime,
    expected: Option<crate::fs_util::FileIdentity>,
    strict: bool,
) -> Result<()> {
    let (root, path) = (root.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let confined = ConfinedPath::open(&root, &path, ConfinedParents::Existing)?;
        let file = confined.open_regular()?;
        let identity = match expected {
            Some(expected) => {
                anyhow::ensure!(
                    crate::fs_util::file_link_count(&file)? == 1,
                    "Primary layout refuses hardlinked media or sidecars"
                );
                anyhow::ensure!(
                    crate::fs_util::file_identity(&file)? == expected,
                    "Prepared primary changed before its time was set"
                );
                expected
            }
            None => crate::fs_util::file_identity(&file)?,
        };
        let file = confined.validate_for_metadata(identity)?;
        file.set_modified(modified)?;
        if file.metadata()?.modified()? != modified {
            anyhow::ensure!(
                !strict,
                "Layout output cannot retain local modification time"
            );
            tracing::warn!(
                target: "kei::download::file",
                path = %confined.path().display(),
                "Filesystem cannot represent the provider edit time exactly; keeping the nearest representable mtime"
            );
        }
        #[cfg(not(windows))]
        file.sync_all()?;
        confined.validate_identity(identity)?;
        Ok(())
    })
    .await?
}

fn snapshot_blocking(root: &Path, path: &Path) -> Result<Option<LayoutFingerprint>> {
    let confined = match ConfinedPath::open(root, path, ConfinedParents::Existing) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    snapshot_confined(&confined)
}

fn snapshot_confined(confined: &ConfinedPath) -> Result<Option<LayoutFingerprint>> {
    snapshot_confined_with_links(confined, 1)
}

/// Snapshot a confined regular entry that is expected to hold `links` names.
fn snapshot_confined_with_links(
    confined: &ConfinedPath,
    links: u64,
) -> Result<Option<LayoutFingerprint>> {
    let Some(mut file) = confined.open_optional_regular()? else {
        return Ok(None);
    };
    let identity = crate::fs_util::file_identity(&file)?;
    anyhow::ensure!(
        crate::fs_util::file_link_count(&file)? == links,
        "Primary layout refuses hardlinked media or sidecars"
    );
    let fingerprint =
        fingerprint_open_file_snapshot_blocking(&mut file, confined.path())?.fingerprint;
    confined.validate_identity(identity)?;
    Ok(Some(LayoutFingerprint {
        size: fingerprint.size,
        sha256: fingerprint.sha256,
        identity,
    }))
}

pub(in crate::download) fn bytes_match(
    left: &LayoutFingerprint,
    right: &LayoutFingerprint,
) -> bool {
    left.size == right.size && left.sha256 == right.sha256
}

/// A failed `open_directory` for a regular file, which moves by link instead.
fn is_regular_entry(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::NotADirectory {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
    }
    #[cfg(not(unix))]
    false
}

/// Move one journaled album entry between public names. A regular file moves by
/// link, with the destination proved before the source name is released. A
/// directory needs the atomic no-replace rename, so a filesystem that rejects it
/// refuses the move instead of installing a name it cannot prove.
pub(in crate::download) async fn move_album_entry(
    root: &Path,
    source: &Path,
    destination: &Path,
    identity: crate::fs_util::FileIdentity,
) -> Result<()> {
    let (root, source, destination) = (root.to_owned(), source.to_owned(), destination.to_owned());
    tokio::task::spawn_blocking(move || {
        let source = ConfinedPath::open(&root, &source, ConfinedParents::Existing)?;
        // Sibling moves share the retained parent capability. A moved user entry
        // lands under the installed album directory, so it opens from the root
        // with creation for any missing ancestor.
        let destination = match destination.parent() {
            Some(parent) if Some(parent) == source.path().parent() => {
                source.sibling(&destination)?
            }
            _ => ConfinedPath::open(&root, &destination, ConfinedParents::Create)?,
        };
        match source.open_directory() {
            Ok(directory) => {
                anyhow::ensure!(
                    crate::fs_util::file_identity(&directory)? == identity,
                    "Album directory identity changed before rename"
                );
                super::platform::move_noreplace_confined(&source, &destination)?;
                let installed = destination.open_directory()?;
                if crate::fs_util::file_identity(&installed)? != identity {
                    // The moved entry is gone from this name. Leave the
                    // substituted entry untouched; its bytes and any retained
                    // copy belong to their current owners.
                    anyhow::bail!("Album directory changed during rename");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The move already landed in an earlier attempt, for a file or a
                // directory alike.
                anyhow::ensure!(
                    destination.retained_entry_identity()? == identity,
                    "Album directory recovery target has another identity"
                );
                super::platform::reclaim_move_slots(&source, &destination, identity)?;
            }
            Err(error) if is_regular_entry(&error) => {
                anyhow::ensure!(
                    source.retained_entry_identity()? == identity,
                    "User album entry changed identity"
                );
                super::platform::move_noreplace_confined(&source, &destination)?;
                if destination.retained_entry_identity()? != identity {
                    super::platform::move_noreplace_confined(&destination, &source)?;
                    anyhow::bail!("User album entry changed during movement");
                }
            }
            Err(error) => return Err(error.into()),
        }
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("album-directory-moved");
        source.sync_parent()?;
        destination.sync_parent()?;
        Ok(())
    })
    .await?
}

pub(in crate::download) async fn verify_album_directory(
    root: &Path,
    path: &Path,
    expected: crate::fs_util::FileIdentity,
) -> Result<()> {
    let (root, path) = (root.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let directory =
            ConfinedPath::open(&root, &path, ConfinedParents::Existing)?.open_directory()?;
        anyhow::ensure!(
            crate::fs_util::file_identity(&directory)? == expected,
            "Album directory identity changed"
        );
        Ok(())
    })
    .await?
}

/// Freeze maximal user subtrees that contain no managed media or sidecars.
/// Private layout history stays at its original receipt paths.
pub(in crate::download) async fn album_user_entries(
    root: &Path,
    directory: &Path,
    files: &[crate::state::db::primary_layout::directory::AlbumDirectoryFile],
) -> Result<Vec<crate::state::db::primary_layout::directory::AlbumUserEntry>> {
    let (root, directory) = (root.to_owned(), directory.to_owned());
    let managed: Vec<_> = files
        .iter()
        .flat_map(|file| {
            let path = file.old.to_path();
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(".xmp");
            [path, PathBuf::from(sidecar)]
        })
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut pending = vec![directory];
        let mut entries = Vec::new();
        while let Some(parent) = pending.pop() {
            for entry in std::fs::read_dir(&parent)? {
                let entry = entry?;
                let path = entry.path();
                if entry.file_name() == ".kei-history" || managed.contains(&path) {
                    continue;
                }
                let directory = entry.file_type()?.is_dir();
                let confined = ConfinedPath::open(&root, &path, ConfinedParents::Existing)?;
                let identity = confined.entry_identity()?;
                if managed.iter().any(|managed| managed.starts_with(&path)) {
                    anyhow::ensure!(directory, "Managed album parent is not a directory");
                    confined.open_directory()?;
                    pending.push(path);
                } else {
                    entries.push(
                        crate::state::db::primary_layout::directory::AlbumUserEntry {
                            path: crate::state::db::provider_selection::SelectionPath::from_path(
                                &path,
                            ),
                            identity,
                        },
                    );
                }
            }
        }
        entries.sort_by_key(|entry| entry.path.to_path());
        Ok(entries)
    })
    .await?
}

pub(in crate::download) async fn copy_verified(
    root: &Path,
    source: &Path,
    destination: &Path,
    expected: &LayoutFingerprint,
) -> Result<LayoutFingerprint> {
    let actual = snapshot(root, source)
        .await?
        .context("Missing primary layout copy source")?;
    anyhow::ensure!(
        actual == *expected,
        "Primary layout source changed before preservation"
    );
    let copy =
        super::reconciliation::copy_local_file_no_replace(root, source, destination, ".kei-layout")
            .await?
            .context("Primary layout preservation destination has different bytes")?;
    copy.validate().await?;
    let result = snapshot(root, destination)
        .await?
        .context("Missing primary layout preserved file")?;
    anyhow::ensure!(
        bytes_match(expected, &result) && result.identity != expected.identity,
        "Primary layout preservation must be an independent exact copy"
    );
    Ok(result)
}

/// Only the layout owner can construct this authorization, after a durable
/// preservation receipt. It is distinct from truncated-repair authorization.
#[derive(Debug)]
pub(in crate::download) struct ManagedPrimaryAuthorization {
    operation: String,
    expected: LayoutFingerprint,
    preserved_path: PathBuf,
    preserved: LayoutFingerprint,
    modified: Option<std::time::SystemTime>,
    publication_time: bool,
    capture_time: Option<std::time::SystemTime>,
}

impl ManagedPrimaryAuthorization {
    pub(in crate::download) fn preserved(
        operation: String,
        expected: LayoutFingerprint,
        path: PathBuf,
        preserved: LayoutFingerprint,
    ) -> Result<Self> {
        anyhow::ensure!(
            !operation.is_empty()
                && bytes_match(&expected, &preserved)
                && expected.identity != preserved.identity,
            "Managed primary replacement requires independent preservation evidence"
        );
        Ok(Self {
            operation,
            expected,
            preserved_path: path,
            preserved,
            modified: None,
            publication_time: false,
            capture_time: None,
        })
    }

    #[must_use]
    pub(in crate::download) fn with_modified(
        mut self,
        modified: Option<std::time::SystemTime>,
    ) -> Self {
        self.modified = modified;
        self
    }

    #[must_use]
    pub(in crate::download) fn with_publication_time(mut self, publication_time: bool) -> Self {
        self.publication_time = publication_time;
        self
    }

    #[must_use]
    pub(in crate::download) fn with_capture_time(
        mut self,
        capture_time: Option<std::time::SystemTime>,
    ) -> Self {
        self.capture_time = capture_time;
        self
    }
}

fn set_modified_file(
    file: &std::fs::File,
    time: std::time::SystemTime,
) -> Result<std::time::SystemTime> {
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(time)
            .set_accessed(time),
    )?;
    #[cfg(not(windows))]
    file.sync_all()?;
    Ok(file.metadata()?.modified()?)
}

/// FAT stores an odd capture second one second lower, so a staged time within
/// two seconds of the capture is not a proven edit clock. Pre-epoch captures
/// keep their old classification.
fn capture_like(prepared: std::time::SystemTime, capture: std::time::SystemTime) -> bool {
    match (
        prepared
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs()),
        capture
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs()),
    ) {
        (Some(prepared), Some(capture)) => prepared.abs_diff(capture) <= 2,
        (None, None) => true,
        _ => false,
    }
}

pub(in crate::download) async fn publish(
    root: &Path,
    prepared: &Path,
    target: &Path,
    new: &LayoutFingerprint,
    authorization: Option<ManagedPrimaryAuthorization>,
) -> Result<LayoutFingerprint> {
    let (root, prepared, target, new) = (
        root.to_owned(),
        prepared.to_owned(),
        target.to_owned(),
        new.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let part=ConfinedPath::open(&root,&prepared,ConfinedParents::Existing)?;
        let destination=ConfinedPath::open(&root,&target,ConfinedParents::Existing)?;
        anyhow::ensure!(snapshot_blocking(&root,&prepared)?.as_ref()==Some(&new),"Prepared primary layout bytes changed");
        if let Some(auth)=authorization {
            let preserved=snapshot_blocking(&root,&auth.preserved_path)?.context("Missing durable preserved primary")?;
            anyhow::ensure!(preserved==auth.preserved,"Preserved primary changed before handover");
            anyhow::ensure!(snapshot_blocking(&root,&target)?.as_ref()==Some(&auth.expected),"Owned primary changed before handover");
            anyhow::ensure!(auth.modified.is_none_or(|time| destination.open_regular().and_then(|file| file.metadata()).and_then(|meta| meta.modified()).ok() == Some(time)), "Owned primary modification time changed before handover");
            // CONTRACT: FILE_PUBLISH_NO_OVERWRITE
            let _operation=auth.operation;
            anyhow::ensure!(snapshot_confined(&part)?.as_ref()==Some(&new) && snapshot_confined(&destination)?.as_ref()==Some(&auth.expected),"Retained primary publication input changed");
            // A replacement must read as changed to consumers that detect changes by mtime:
            // a staged edit time newer than its predecessor is kept, else a publication
            // time past the predecessor is used. A staged time within the capture window
            // is not a proven edit clock, so it always takes the publication time; the
            // window matches the two-second floor because FAT can store an odd capture
            // second one second lower. A preserved local copy keeps its recorded mtime.
            if auth.publication_time {
                let predecessor=destination.open_regular()?.metadata()?.modified()?;
                let prepared_file=part.validate_for_metadata(new.identity)?;
                let prepared_modified=prepared_file.metadata()?.modified()?;
                let staged_is_capture_like=auth.capture_time.is_some_and(|capture| capture_like(prepared_modified,capture));
                if staged_is_capture_like || prepared_modified<=predecessor {
                    // A two-second floor clears whole-second and FAT two-second truncation.
                    let floor=predecessor.checked_add(std::time::Duration::from_secs(2)).unwrap_or_else(std::time::SystemTime::now);
                    let stored=set_modified_file(&prepared_file,std::time::SystemTime::now().max(floor))?;
                    anyhow::ensure!(stored>predecessor,"Cannot store a publication time newer than the predecessor");
                }
                drop(prepared_file);
                anyhow::ensure!(snapshot_confined(&part)?.as_ref()==Some(&new),"Prepared primary layout bytes changed");
            }
            #[cfg(unix)]
            {
                #[cfg(all(test,target_os="linux"))]
                let force_journal=crate::test_helpers::process_death_force_journal();
                #[cfg(not(all(test,target_os="linux")))]
                let force_journal=false;
                let exchange=if force_journal {Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP))} else {super::platform::exchange_layout_confined(&part,&destination)};
                #[cfg(target_os="linux")]
                let exchange=if exchange.as_ref().err().is_some_and(super::platform::is_renameat2_unsupported) {
                    super::replacement_recovery::publish_confined(&part,&destination,
                        ExistingFileFingerprint{size:auth.expected.size,sha256:auth.expected.sha256},
                        ExistingFileFingerprint{size:new.size,sha256:new.sha256},auth.modified)?;
                    None
                } else {Some(exchange)};
                #[cfg(not(target_os="linux"))]
                let exchange=Some(exchange);
                if let Some(exchange)=exchange {
                exchange?;
                let displaced=snapshot_confined(&part);
                let installed=snapshot_confined(&destination);
                let displaced=displaced.as_ref().ok().and_then(|file|file.as_ref());
                let installed=installed.as_ref().ok().and_then(|file|file.as_ref());
                let time_matches = auth.modified.is_none_or(|time| part.open_regular().and_then(|file| file.metadata()).and_then(|meta| meta.modified()).ok() == Some(time));
                if displaced!=Some(&auth.expected) || installed!=Some(&new) || !time_matches {
                    // Restore only while the installed entry still has our
                    // exact identity and bytes. Every displaced entry remains
                    // retained in either slot, including a racing foreign file.
                    if installed==Some(&new) {super::platform::exchange_layout_confined(&part,&destination)?;}
                    anyhow::bail!("Primary entry changed during confined exchange; displaced bytes retained or restored");
                }
                // Keep the displaced staging entry. The independent history
                // receipt, rather than removal of this entry, proves preservation.
                part.sync_parent()?;
                }
            }
            #[cfg(windows)]
            {
                let _prepared_pin=part.pin_identity(new.identity)?;
                let _target_pin=destination.pin_identity(auth.expected.identity)?;
                let expected = ExistingFileFingerprint{size:auth.expected.size,sha256:auth.expected.sha256};
                let replacement = ExistingFileFingerprint{size:new.size,sha256:new.sha256};
                if let Some(modified) = auth.modified {
                    super::replacement::publish_file_if_unchanged_at_mtime_blocking(part.path(),destination.path(),expected,replacement,modified)?;
                } else {
                    super::replacement::publish_file_if_unchanged_blocking(part.path(),destination.path(),expected,replacement)?;
                }
            }
        } else {
            match super::platform::publish_reconciliation_part_blocking(&part,&destination)? {
                super::platform::PublishResult::Published=>{},
                super::platform::PublishResult::DestinationExists=>anyhow::bail!("Foreign file appeared in a primary layout destination"),
            }
        }
        #[cfg(target_os="linux")]
        super::replacement_recovery::finish_owned_prepared_link(&part,&destination,
            ExistingFileFingerprint{size:new.size,sha256:new.sha256},new.identity)?;
        destination.sync_parent()?;
        let result=snapshot_confined(&destination)?.context("Missing installed primary layout file")?;
        anyhow::ensure!(bytes_match(&new,&result),"Primary layout publication changed before verification");
        Ok(result)
    }).await?
}

/// Recover the low-level owner's leftover prepared link using the layout's
/// independently durable receipt, before ordinary hardlink refusal applies.
pub(in crate::download) async fn recover_prepared_link(
    root: &Path,
    stage: &Path,
    target: &Path,
    new: &LayoutFingerprint,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let (root, stage, target, new) = (
            root.to_owned(),
            stage.to_owned(),
            target.to_owned(),
            new.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let part = ConfinedPath::open(&root, &stage, ConfinedParents::Existing)?;
            let target = ConfinedPath::open(&root, &target, ConfinedParents::Existing)?;
            super::replacement_recovery::finish_owned_prepared_link(
                &part,
                &target,
                ExistingFileFingerprint {
                    size: new.size,
                    sha256: new.sha256,
                },
                new.identity,
            )
        })
        .await??;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (root, stage, target, new);
    Ok(())
}

/// The sidecar disposition pinned by the layout journal, including absence.
pub(in crate::download) struct RetirementCompanion<'a> {
    pub(crate) path: &'a Path,
    pub(crate) retired: &'a Path,
    /// None is an owned absence requirement, not permission to ignore a file.
    pub(crate) expected: Option<&'a LayoutFingerprint>,
}

/// Retire a journaled alias and its optional companion as one guarded operation.
/// Concurrent entries are restored without replacing later writers, or retained
/// with an error. No stat-then-unlink and no pruning is used.
pub(in crate::download) async fn retire(
    root: &Path,
    path: &Path,
    retired: &Path,
    expected: &LayoutFingerprint,
    companion: Option<RetirementCompanion<'_>>,
) -> Result<()> {
    let (root, path, retired, expected) = (
        root.to_owned(),
        path.to_owned(),
        retired.to_owned(),
        expected.clone(),
    );
    let companion = companion.map(|file| {
        (
            file.path.to_owned(),
            file.retired.to_owned(),
            file.expected.cloned(),
        )
    });
    tokio::task::spawn_blocking(move || {
        let mut entries = vec![RetirementEntry::open(
            &root,
            &path,
            &retired,
            Some(expected),
        )?];
        if let Some((path, retired, expected)) = companion {
            match RetirementEntry::open(&root, &path, &retired, expected) {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    restore_retirement(&entries);
                    return Err(error);
                }
            }
        }
        retire_blocking(&entries)
    })
    .await?
}

struct RetirementEntry {
    source: ConfinedPath,
    destination: ConfinedPath,
    expected: Option<LayoutFingerprint>,
}

impl RetirementEntry {
    fn open(
        root: &Path,
        path: &Path,
        retired: &Path,
        expected: Option<LayoutFingerprint>,
    ) -> Result<Self> {
        Ok(Self {
            source: ConfinedPath::open(root, path, ConfinedParents::Existing)?,
            destination: ConfinedPath::open(root, retired, ConfinedParents::Create)?,
            expected,
        })
    }

    /// A resumed move is complete only with exact saved bytes and an empty
    /// original slot. Every companion, including expected absence, participates.
    fn is_retired(&self) -> Result<bool> {
        // A split link-based retirement is resumable only for a member the
        // journal recorded as present, and only while the slot still holds that
        // exact entry. An owned absence is never resumed into a move.
        if self.expected.is_some()
            && let (Ok(source), Ok(saved)) = (
                self.source.retained_entry_identity(),
                self.destination.retained_entry_identity(),
            )
            && source == saved
        {
            let saved = snapshot_confined_with_links(&self.destination, 2)?;
            anyhow::ensure!(
                saved.as_ref() == self.expected.as_ref(),
                "Retired member changed during a split retirement"
            );
            return Ok(false);
        }
        let source = snapshot_confined(&self.source)?;
        let saved = snapshot_confined(&self.destination)?;
        match (&self.expected, source.as_ref(), saved.as_ref()) {
            (None, None, None) => Ok(true),
            (Some(expected), Some(actual), None) if actual == expected => Ok(false),
            (Some(expected), None, Some(actual)) if actual == expected => Ok(true),
            _ => anyhow::bail!("Media or sidecar disposition changed during layout retirement"),
        }
    }
}

fn retire_blocking(entries: &[RetirementEntry]) -> Result<()> {
    let result = (|| {
        for (index, entry) in entries.iter().enumerate() {
            // Check the whole pair before each move, including on journal reopen.
            let states = entries
                .iter()
                .map(RetirementEntry::is_retired)
                .collect::<Result<Vec<_>>>()?;
            if states
                .get(index)
                .copied()
                .context("Retirement member disappeared")?
            {
                continue;
            }
            #[cfg(test)]
            tests::retirement_point(if index == 0 {
                "before-media"
            } else {
                "before-sidecar"
            });
            super::platform::retire_layout_confined(&entry.source, &entry.destination)?;
            entry.source.sync_parent()?;
            entry.destination.sync_parent()?;
            #[cfg(all(test, target_os = "linux"))]
            if index == 0 {
                crate::test_helpers::process_death_point("layout-retirement-media-moved");
            }
            #[cfg(test)]
            tests::retirement_point(if index == 0 {
                "after-media"
            } else {
                "after-sidecar"
            });
            // The no-replace rename guards the moved entry; this postcheck also
            // catches a companion/namespace writer between validation and rename.
            for candidate in entries {
                candidate.is_retired()?;
            }
            anyhow::ensure!(
                entry.is_retired()?,
                "Retirement source reappeared during move"
            );
        }
        for entry in entries {
            anyhow::ensure!(entry.is_retired()?, "Layout retirement is incomplete");
        }
        Ok(())
    })();
    if result.is_err() {
        restore_retirement(entries);
    }
    result
}

fn restore_retirement(entries: &[RetirementEntry]) {
    // Restore media before metadata, including moves from a prior process.
    // Retained directory capabilities prevent following a swapped parent.
    // Never replace a later writer; any failed restoration retains the saved
    // entry and the caller's durable journal for another recovery attempt.
    for entry in entries.iter().filter(|entry| entry.expected.is_some()) {
        if entry.destination.entry_exists().unwrap_or(false)
            && !entry.source.entry_exists().unwrap_or(true)
        {
            let restored =
                super::platform::move_noreplace_confined(&entry.destination, &entry.source);
            let durable = entry
                .source
                .sync_parent()
                .and_then(|()| entry.destination.sync_parent());
            if restored.is_err() || durable.is_err() {
                tracing::warn!(
                    "Layout retirement restoration remains unresolved; journal and saved entries retained"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedPrimaryAuthorization, RetirementEntry, capture_like, copy_verified, publish,
        retire_blocking, snapshot, snapshot_blocking,
    };
    use std::cell::RefCell;

    type RetirementMutation = (&'static str, Box<dyn FnOnce()>);
    thread_local! {
        static RETIREMENT_MUTATION: RefCell<Option<RetirementMutation>> = const { RefCell::new(None) };
    }

    pub(super) fn retirement_point(point: &str) {
        let mutation = RETIREMENT_MUTATION.with(|hook| {
            let mut hook = hook.borrow_mut();
            if hook.as_ref().is_some_and(|(at, _)| *at == point) {
                hook.take()
            } else {
                None
            }
        });
        if let Some((_, mutate)) = mutation {
            mutate();
        }
    }

    #[test]
    fn retirement_pair_restores_after_companion_races_and_keeps_later_media_writer() {
        for (point, owned, media_writer) in [
            ("before-media", false, false),
            ("after-media", false, false),
            ("before-media", true, false),
            ("before-sidecar", true, false),
            ("after-sidecar", true, false),
            ("after-media", false, true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let media = root.path().join("photo.jpg");
            let sidecar = root.path().join("photo.jpg.xmp");
            let retired = root.path().join("private/retired");
            let retired_sidecar = root.path().join("private/retired.xmp");
            std::fs::write(&media, b"owned media").unwrap();
            if owned {
                std::fs::write(&sidecar, b"owned sidecar").unwrap();
            }
            let entries = [
                RetirementEntry::open(
                    root.path(),
                    &media,
                    &retired,
                    snapshot_blocking(root.path(), &media).unwrap(),
                )
                .unwrap(),
                RetirementEntry::open(
                    root.path(),
                    &sidecar,
                    &retired_sidecar,
                    snapshot_blocking(root.path(), &sidecar).unwrap(),
                )
                .unwrap(),
            ];
            let changed = if point == "after-sidecar" {
                retired_sidecar.clone()
            } else {
                sidecar.clone()
            };
            let writer_media = media.clone();
            RETIREMENT_MUTATION.with(|hook| {
                *hook.borrow_mut() = Some((
                    point,
                    Box::new(move || {
                        std::fs::write(changed, b"user sidecar").unwrap();
                        if media_writer {
                            std::fs::write(writer_media, b"later media writer").unwrap();
                        }
                    }),
                ))
            });
            assert!(
                retire_blocking(&entries).is_err(),
                "{point}/{owned}/{media_writer}"
            );
            RETIREMENT_MUTATION
                .with(|hook| assert!(hook.borrow().is_none(), "race seam did not fire"));
            assert_eq!(std::fs::read(&sidecar).unwrap(), b"user sidecar");
            if media_writer {
                assert_eq!(std::fs::read(&media).unwrap(), b"later media writer");
                assert_eq!(std::fs::read(&retired).unwrap(), b"owned media");
            } else {
                assert_eq!(std::fs::read(&media).unwrap(), b"owned media");
                assert!(!retired.exists());
            }
            assert!(!retired_sidecar.exists());
            assert!(retire_blocking(&entries).is_err());
            assert_eq!(std::fs::read(&sidecar).unwrap(), b"user sidecar");
        }
    }

    #[cfg(unix)]
    #[test]
    fn retirement_pair_parent_swap_restores_in_retained_namespace_without_touching_new_parent() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("album");
        let moved = root.path().join("moved-album");
        std::fs::create_dir(&parent).unwrap();
        let media = parent.join("photo.jpg");
        let sidecar = parent.join("photo.jpg.xmp");
        let retired = root.path().join("private/retired");
        let retired_sidecar = root.path().join("private/retired.xmp");
        std::fs::write(&media, b"owned media").unwrap();
        let entries = [
            RetirementEntry::open(
                root.path(),
                &media,
                &retired,
                snapshot_blocking(root.path(), &media).unwrap(),
            )
            .unwrap(),
            RetirementEntry::open(root.path(), &sidecar, &retired_sidecar, None).unwrap(),
        ];
        let (old_parent, moved_parent) = (parent.clone(), moved.clone());
        RETIREMENT_MUTATION.with(|hook| {
            *hook.borrow_mut() = Some((
                "before-media",
                Box::new(move || {
                    std::fs::rename(&old_parent, &moved_parent).unwrap();
                    std::fs::create_dir(&old_parent).unwrap();
                    std::fs::write(old_parent.join("photo.jpg"), b"foreign namespace media")
                        .unwrap();
                    std::fs::write(
                        old_parent.join("photo.jpg.xmp"),
                        b"foreign namespace sidecar",
                    )
                    .unwrap();
                    std::fs::write(moved_parent.join("photo.jpg.xmp"), b"late user sidecar")
                        .unwrap();
                }),
            ))
        });
        assert!(retire_blocking(&entries).is_err());
        RETIREMENT_MUTATION.with(|hook| assert!(hook.borrow().is_none()));
        assert_eq!(
            std::fs::read(moved.join("photo.jpg")).unwrap(),
            b"owned media"
        );
        assert_eq!(
            std::fs::read(moved.join("photo.jpg.xmp")).unwrap(),
            b"late user sidecar"
        );
        assert_eq!(std::fs::read(media).unwrap(), b"foreign namespace media");
        assert_eq!(
            std::fs::read(sidecar).unwrap(),
            b"foreign namespace sidecar"
        );
        assert!(!retired.exists());
    }

    #[cfg(target_os = "linux")]
    async fn assert_retire_moves_alias_to_its_private_slot(root: &std::path::Path) {
        use super::retire;
        use std::os::unix::fs::MetadataExt;

        let alias = root.join("current.jpg");
        let slot = root.join("history/.retired-slot");
        tokio::fs::write(&alias, b"owned alias bytes")
            .await
            .unwrap();
        let expected = snapshot(root, &alias).await.unwrap().unwrap();
        retire(root, &alias, &slot, &expected, None).await.unwrap();
        assert!(!alias.exists());
        assert_eq!(tokio::fs::read(&slot).await.unwrap(), b"owned alias bytes");
        assert_eq!(std::fs::metadata(&slot).unwrap().nlink(), 1);
        // Replaying a completed retirement is a no-op.
        retire(root, &alias, &slot, &expected, None).await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn retire_uses_a_plain_rename_when_noreplace_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let _fallback = super::super::platform::ForcedLinkFallback::new(dir.path());
        assert_retire_moves_alias_to_its_private_slot(dir.path()).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nfs_retire_moves_alias_to_its_private_slot() {
        let Some(dir) = crate::test_helpers::nfs_test_dir("nfs_retire") else {
            return;
        };
        assert_retire_moves_alias_to_its_private_slot(dir.path()).await;
    }

    #[cfg(target_os = "linux")]
    async fn assert_album_directory_move_round_trips(root: &std::path::Path) {
        use super::move_album_entry;
        use crate::fs_util::{ConfinedParents, ConfinedPath};

        let current = root.join("media/Old");
        let next = root.join("media/New");
        std::fs::create_dir_all(current.join("user")).unwrap();
        std::fs::write(current.join("user/note.txt"), b"user bytes").unwrap();
        let identity = ConfinedPath::open(root, &current, ConfinedParents::Existing)
            .unwrap()
            .entry_identity()
            .unwrap();
        move_album_entry(root, &current, &next, identity)
            .await
            .unwrap();
        assert!(!current.exists());
        assert_eq!(
            std::fs::read(next.join("user/note.txt")).unwrap(),
            b"user bytes"
        );
        assert_eq!(
            ConfinedPath::open(root, &next, ConfinedParents::Existing)
                .unwrap()
                .entry_identity()
                .unwrap(),
            identity
        );
        assert_eq!(
            std::fs::read_dir(root.join("media")).unwrap().count(),
            1,
            "the move leaves exactly one directory name"
        );
        move_album_entry(root, &next, &current, identity)
            .await
            .unwrap();
        assert!(!next.exists());
        assert_eq!(
            std::fs::read(current.join("user/note.txt")).unwrap(),
            b"user bytes"
        );
        assert_eq!(
            ConfinedPath::open(root, &current, ConfinedParents::Existing)
                .unwrap()
                .entry_identity()
                .unwrap(),
            identity
        );
        assert_eq!(
            std::fs::read_dir(root.join("media")).unwrap().count(),
            1,
            "the restore leaves exactly one directory name"
        );
    }

    /// True when the scratch filesystem accepts `RENAME_NOREPLACE`, which a
    /// directory move needs. The scratch directory may itself be the export.
    #[cfg(target_os = "linux")]
    fn directory_move_supported(root: &std::path::Path) -> bool {
        crate::test_helpers::directory_rename_supported_in(root)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn album_directory_move_round_trips_with_noreplace() {
        let root = tempfile::tempdir().unwrap();
        if !directory_move_supported(root.path()) {
            return;
        }
        assert_album_directory_move_round_trips(root.path()).await;
    }

    #[cfg(target_os = "linux")]
    async fn assert_album_directory_move_refuses(root: &std::path::Path) {
        use super::move_album_entry;
        use crate::fs_util::{ConfinedParents, ConfinedPath};

        let current = root.join("media/Old");
        let next = root.join("media/New");
        std::fs::create_dir_all(current.join("user")).unwrap();
        std::fs::write(current.join("user/note.txt"), b"user bytes").unwrap();
        let identity = ConfinedPath::open(root, &current, ConfinedParents::Existing)
            .unwrap()
            .entry_identity()
            .unwrap();
        let error = move_album_entry(root, &current, &next, identity)
            .await
            .expect_err("a directory move needs an atomic no-replace rename");
        assert!(
            format!("{error:#}").contains("atomic no-replace"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read(current.join("user/note.txt")).unwrap(),
            b"user bytes"
        );
        assert!(!next.exists(), "no destination name is created");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn album_directory_move_refuses_when_noreplace_is_unsupported() {
        let root = tempfile::tempdir().unwrap();
        let _fallback = super::super::platform::ForcedLinkFallback::new(root.path());
        assert_album_directory_move_refuses(root.path()).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn nfs_album_directory_move_refuses_without_atomic_no_replace() {
        let Some(root) = crate::test_helpers::nfs_test_dir("nfs_album_directory_move") else {
            return;
        };
        assert_album_directory_move_refuses(root.path()).await;
    }

    // The directory claim mechanism is gone: a filesystem without an atomic
    // no-replace rename refuses the move, covered by the refusal tests above.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn album_directory_move_refuses_an_occupied_destination_when_noreplace_is_unsupported() {
        use super::move_album_entry;
        use crate::fs_util::{ConfinedParents, ConfinedPath};

        let root = tempfile::tempdir().unwrap();
        let _fallback = super::super::platform::ForcedLinkFallback::new(root.path());
        let current = root.path().join("media/Old");
        let next = root.path().join("media/New");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("photo.jpg"), b"owned media").unwrap();
        std::fs::create_dir(&next).unwrap();
        std::fs::write(next.join("foreign.txt"), b"foreign bytes").unwrap();
        let identity = ConfinedPath::open(root.path(), &current, ConfinedParents::Existing)
            .unwrap()
            .entry_identity()
            .unwrap();
        assert!(
            move_album_entry(root.path(), &current, &next, identity)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(current.join("photo.jpg")).unwrap(),
            b"owned media"
        );
        assert_eq!(
            std::fs::read(next.join("foreign.txt")).unwrap(),
            b"foreign bytes"
        );
        assert_eq!(
            std::fs::read_dir(&next).unwrap().count(),
            1,
            "the occupied destination is untouched"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn album_user_entry_move_links_and_verifies_when_noreplace_is_unsupported() {
        use super::move_album_entry;
        use crate::fs_util::{ConfinedParents, ConfinedPath};
        use std::os::unix::fs::MetadataExt;

        let root = tempfile::tempdir().unwrap();
        let _fallback = super::super::platform::ForcedLinkFallback::new(root.path());
        let current = root.path().join("media/Old");
        let next = root.path().join("media/New");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(&next).unwrap();
        std::fs::write(current.join("note.txt"), b"user bytes").unwrap();
        let source = ConfinedPath::open(
            root.path(),
            &current.join("note.txt"),
            ConfinedParents::Existing,
        )
        .unwrap();
        let identity = source.entry_identity().unwrap();
        move_album_entry(root.path(), source.path(), &next.join("note.txt"), identity)
            .await
            .unwrap();
        assert!(!current.join("note.txt").exists());
        assert_eq!(std::fs::read(next.join("note.txt")).unwrap(), b"user bytes");
        assert_eq!(
            ConfinedPath::open(
                root.path(),
                &next.join("note.txt"),
                ConfinedParents::Existing
            )
            .unwrap()
            .entry_identity()
            .unwrap(),
            identity
        );
        assert_eq!(
            std::fs::metadata(next.join("note.txt")).unwrap().nlink(),
            1,
            "the moved file keeps one name"
        );
        assert_eq!(std::fs::read_dir(&current).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&next).unwrap().count(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retirement_restore_links_and_verifies_when_noreplace_is_unsupported() {
        use super::restore_retirement;
        use std::os::unix::fs::MetadataExt;

        let root = tempfile::tempdir().unwrap();
        let _fallback = super::super::platform::ForcedLinkFallback::new(root.path());
        let media = root.path().join("photo.jpg");
        let retired = root.path().join("private/retired");
        std::fs::create_dir(root.path().join("private")).unwrap();
        std::fs::write(&retired, b"saved media").unwrap();
        let expected = snapshot_blocking(root.path(), &retired).unwrap();
        let entries = [RetirementEntry::open(root.path(), &media, &retired, expected).unwrap()];
        restore_retirement(&entries);
        assert!(!retired.exists());
        assert_eq!(std::fs::read(&media).unwrap(), b"saved media");
        assert_eq!(
            std::fs::metadata(&media).unwrap().nlink(),
            1,
            "the restored entry keeps one name"
        );
    }

    #[test]
    fn capture_like_covers_coarse_and_pre_epoch_times() {
        use std::time::{Duration, UNIX_EPOCH};
        assert!(capture_like(
            UNIX_EPOCH + Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_secs(12)
        ));
        assert!(capture_like(
            UNIX_EPOCH + Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_secs(9)
        ));
        assert!(!capture_like(
            UNIX_EPOCH + Duration::from_secs(13),
            UNIX_EPOCH + Duration::from_secs(10)
        ));
        assert!(capture_like(
            UNIX_EPOCH - Duration::from_secs(1),
            UNIX_EPOCH - Duration::from_secs(2)
        ));
        assert!(!capture_like(
            UNIX_EPOCH - Duration::from_secs(1),
            UNIX_EPOCH + Duration::from_secs(1)
        ));
    }

    #[tokio::test]
    async fn contract_file_publish_no_overwrite_primary_layout_preserves_and_protects_late_writer()
    {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("current.jpg");
        let stage = root.path().join("prepared.jpg");
        let history = root.path().join("history/old.jpg");
        tokio::fs::write(&current, b"old owned bytes")
            .await
            .unwrap();
        tokio::fs::write(&stage, b"new verified bytes")
            .await
            .unwrap();
        let old = snapshot(root.path(), &current).await.unwrap().unwrap();
        let new = snapshot(root.path(), &stage).await.unwrap().unwrap();
        let saved = copy_verified(root.path(), &current, &history, &old)
            .await
            .unwrap();
        assert_ne!(saved.identity, old.identity);
        tokio::fs::write(&current, b"late foreign writer")
            .await
            .unwrap();
        let authorization = ManagedPrimaryAuthorization::preserved(
            "operation".into(),
            old.clone(),
            history.clone(),
            saved.clone(),
        )
        .unwrap();
        assert!(
            publish(root.path(), &stage, &current, &new, Some(authorization))
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read(&current).await.unwrap(),
            b"late foreign writer"
        );
        assert_eq!(tokio::fs::read(&history).await.unwrap(), b"old owned bytes");
        assert_eq!(
            tokio::fs::read(&stage).await.unwrap(),
            b"new verified bytes"
        );
    }

    #[tokio::test]
    async fn contract_file_publish_no_overwrite_primary_layout_advances_a_coarse_capture_time() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("current.jpg");
        let stage = root.path().join("prepared.jpg");
        let history = root.path().join("history/old.jpg");
        tokio::fs::write(&current, b"old owned bytes")
            .await
            .unwrap();
        tokio::fs::write(&stage, b"new verified bytes")
            .await
            .unwrap();
        let capture = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_001);
        let predecessor = capture - std::time::Duration::from_secs(60);
        let stored_capture = capture - std::time::Duration::from_secs(1);
        for (path, time) in [(&current, predecessor), (&stage, stored_capture)] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
        let old = snapshot(root.path(), &current).await.unwrap().unwrap();
        let new = snapshot(root.path(), &stage).await.unwrap().unwrap();
        let saved = copy_verified(root.path(), &current, &history, &old)
            .await
            .unwrap();
        let authorization =
            ManagedPrimaryAuthorization::preserved("operation".into(), old, history, saved)
                .unwrap()
                .with_publication_time(true)
                .with_capture_time(Some(capture));
        let installed = publish(root.path(), &stage, &current, &new, Some(authorization))
            .await
            .unwrap();
        assert_eq!(installed.sha256, new.sha256);
        let modified = std::fs::metadata(&current).unwrap().modified().unwrap();
        assert!(
            modified > capture,
            "a coarse capture time must advance the capture"
        );
        assert_ne!(
            modified, stored_capture,
            "a capture-like staged time must not be kept"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn primary_layout_refuses_symlink_and_hardlink_inputs_and_roundtrips_native_paths() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let media = outside.path().join("user.jpg");
        tokio::fs::write(&media, b"outside user media")
            .await
            .unwrap();
        let linked = root.path().join("linked.jpg");
        std::os::unix::fs::symlink(&media, &linked).unwrap();
        assert!(snapshot(root.path(), &linked).await.is_err());
        let parent = root.path().join("foreign");
        std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
        assert!(
            snapshot(root.path(), &parent.join("user.jpg"))
                .await
                .is_err()
        );
        let native = root.path().join(std::ffi::OsString::from_vec(vec![
            b'm', 0xff, b'.', b'j', b'p', b'g',
        ]));
        let receipt = crate::state::db::provider_selection::SelectionPath::from_path(&native);
        let encoded = serde_json::to_vec(&receipt).unwrap();
        let decoded: crate::state::db::provider_selection::SelectionPath =
            serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.to_path(), native);
        // The macOS fixture filesystem rejects non-UTF-8 names. Keep the receipt oracle
        // above on every Unix platform and use a supported physical filename
        // for the macOS snapshot and hardlink guards.
        #[cfg(target_os = "macos")]
        let physical = root.path().join("native.jpg");
        #[cfg(not(target_os = "macos"))]
        let physical = decoded.to_path();
        tokio::fs::write(&physical, b"native exact bytes")
            .await
            .unwrap();
        assert!(snapshot(root.path(), &physical).await.unwrap().is_some());
        std::fs::hard_link(&physical, root.path().join("hardlink.jpg")).unwrap();
        assert!(snapshot(root.path(), &physical).await.is_err());
        assert_eq!(tokio::fs::read(media).await.unwrap(), b"outside user media");
    }
}
