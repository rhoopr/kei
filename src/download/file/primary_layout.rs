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

fn snapshot_blocking(root: &Path, path: &Path) -> Result<Option<LayoutFingerprint>> {
    let confined = match ConfinedPath::open(root, path, ConfinedParents::Existing) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    snapshot_confined(&confined)
}

fn snapshot_confined(confined: &ConfinedPath) -> Result<Option<LayoutFingerprint>> {
    let Some(mut file) = confined.open_optional_regular()? else {
        return Ok(None);
    };
    let identity = crate::fs_util::file_identity(&file)?;
    anyhow::ensure!(
        crate::fs_util::file_link_count(&file)? == 1,
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
        })
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
            // CONTRACT: FILE_PUBLISH_NO_OVERWRITE
            let _operation=auth.operation;
            anyhow::ensure!(snapshot_confined(&part)?.as_ref()==Some(&new) && snapshot_confined(&destination)?.as_ref()==Some(&auth.expected),"Retained primary publication input changed");
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
                        ExistingFileFingerprint{size:new.size,sha256:new.sha256})?;
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
                if displaced!=Some(&auth.expected) || installed!=Some(&new) {
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
                super::replacement::publish_file_if_unchanged_blocking(part.path(),destination.path(),
                    ExistingFileFingerprint{size:auth.expected.size,sha256:auth.expected.sha256},
                    ExistingFileFingerprint{size:new.size,sha256:new.sha256})?;
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

/// Move an obsolete alias into a journaled private retirement slot. A displaced
/// concurrent entry is restored without replacing any later writer, or retained
/// with an error. No stat-then-unlink and no pruning is used.
pub(in crate::download) async fn retire(
    root: &Path,
    path: &Path,
    retired: &Path,
    expected: &LayoutFingerprint,
) -> Result<()> {
    let (root, path, retired, expected) = (
        root.to_owned(),
        path.to_owned(),
        retired.to_owned(),
        expected.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let source=ConfinedPath::open(&root,&path,ConfinedParents::Existing)?;
        let destination=ConfinedPath::open(&root,&retired,ConfinedParents::Create)?;
        if snapshot_blocking(&root,&path)?.is_none() {
            let saved=snapshot_blocking(&root,&retired)?.context("Retired alias lacks recovery evidence")?;
            anyhow::ensure!(saved==expected,"Retired alias differs from journal");
            return Ok(());
        }
        anyhow::ensure!(snapshot_blocking(&root,&path)?.as_ref()==Some(&expected),"Obsolete primary alias changed before retirement");
        super::platform::move_layout_confined(&source,&destination)?;
        let displaced=snapshot_blocking(&root,&retired);
        if displaced.as_ref().ok().and_then(|file|file.as_ref())!=Some(&expected) {
            let restore=super::platform::move_layout_confined(&destination,&source);
            return Err(anyhow::anyhow!("Obsolete alias changed during retirement; displaced bytes retained or restored: {}",restore.is_ok()));
        }
        source.sync_parent()?;destination.sync_parent()?;Ok(())
    }).await?
}

#[cfg(test)]
mod tests {
    use super::{ManagedPrimaryAuthorization, copy_verified, publish, snapshot};
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
        tokio::fs::write(&native, b"native exact bytes")
            .await
            .unwrap();
        let receipt = crate::state::db::provider_selection::SelectionPath::from_path(&native);
        let encoded = serde_json::to_vec(&receipt).unwrap();
        let decoded: crate::state::db::provider_selection::SelectionPath =
            serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.to_path(), native);
        assert!(
            snapshot(root.path(), &decoded.to_path())
                .await
                .unwrap()
                .is_some()
        );
        std::fs::hard_link(&native, root.path().join("hardlink.jpg")).unwrap();
        assert!(snapshot(root.path(), &native).await.is_err());
        assert_eq!(tokio::fs::read(media).await.unwrap(), b"outside user media");
    }
}
