use super::{
    Fingerprint, Journal, Manifest, Stage, confined, directory_path, publish, publish_journal,
    recover_target,
};
use crate::download::file::fingerprint::fingerprint_regular_file_blocking;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use tempfile::TempDir;

fn prepare(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, Journal) {
    let target = dir.join("photo.jpg");
    let part = dir.join("photo.part");
    fs::write(&target, b"original").unwrap();
    fs::write(&part, b"replacement").unwrap();
    let journal = Journal::create(
        &confined(&part).unwrap(),
        confined(&target).unwrap(),
        fingerprint_regular_file_blocking(&target).unwrap(),
        fingerprint_regular_file_blocking(&part).unwrap(),
    )
    .unwrap();
    (target, part, journal)
}

#[test]
fn local_policy_journal_checks_displaced_mtime_and_restores_on_race() {
    let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    let changed = expected + std::time::Duration::from_secs(60);
    for stage in [Stage::Prepared, Stage::Displaced, Stage::Installed] {
        let dir = TempDir::new().unwrap();
        let (target, part, mut journal) = prepare(dir.path());
        fs::File::open(&target)
            .unwrap()
            .set_modified(expected)
            .unwrap();
        assert!(
            super::publish_journal_with_mtime(&journal, Some(expected), |current| {
                if current == stage {
                    let path = if current == Stage::Prepared {
                        &target
                    } else {
                        journal.original.path()
                    };
                    fs::File::open(path)?.set_modified(changed)?;
                }
                Ok(())
            })
            .is_err()
        );
        assert!(!journal.commit.path().exists());
        journal.recover().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), changed);
        assert_eq!(fs::read(&part).unwrap(), b"replacement");
        drop(journal);
        recover_target(&target).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), changed);
    }
}

#[test]
fn local_policy_journal_publishes_with_exact_time_and_recovers_interruption() {
    let expected = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for stage in [
        Stage::Prepared,
        Stage::Displaced,
        Stage::Installed,
        Stage::Committed,
    ] {
        let dir = TempDir::new().unwrap();
        let (target, part, journal) = prepare(dir.path());
        fs::File::open(&target)
            .unwrap()
            .set_modified(expected)
            .unwrap();
        assert!(
            super::publish_journal_with_mtime(&journal, Some(expected), |current| {
                anyhow::ensure!(current != stage, "interrupted fixture");
                Ok(())
            })
            .is_err()
        );
        drop(journal);
        recover_target(&target).unwrap();
        if stage == Stage::Committed {
            assert_eq!(fs::read(&target).unwrap(), b"replacement");
        } else {
            assert_eq!(fs::read(&target).unwrap(), b"original");
            assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), expected);
            super::publish_with_mtime(
                &part,
                &target,
                fingerprint_regular_file_blocking(&target).unwrap(),
                fingerprint_regular_file_blocking(&part).unwrap(),
                Some(expected),
            )
            .unwrap();
            assert_eq!(fs::read(&target).unwrap(), b"replacement");
        }
        recover_target(&target).unwrap();
        assert!(!directory_path(&target).unwrap().exists());
    }
}

#[test]
fn manifest_round_trip_preserves_non_utf8_target_and_fingerprints() {
    let expected = Manifest {
        version: super::FORMAT_VERSION,
        target: b"photo-\xff.jpg".to_vec(),
        original: Fingerprint {
            size: 4,
            sha256: [3; 32],
        },
        replacement: Fingerprint {
            size: 9,
            sha256: [8; 32],
        },
    };
    let bytes = serde_json::to_vec(&expected).unwrap();
    assert_eq!(
        serde_json::from_slice::<Manifest>(&bytes).unwrap(),
        expected
    );
    assert_eq!(
        directory_path(std::path::Path::new(&std::ffi::OsString::from_vec(
            expected.target
        )))
        .unwrap()
        .file_name()
        .unwrap()
        .len(),
        super::JOURNAL_PREFIX.len() + 64
    );
}

#[test]
fn concurrent_edit_before_displacement_is_restored_and_retained() {
    let dir = TempDir::new().unwrap();
    let (target, part, mut journal) = prepare(dir.path());
    let error = publish_journal(&journal, |stage| {
        if stage == Stage::Prepared {
            fs::write(&target, b"user edit")?;
        }
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("bytes changed"));
    assert!(journal.recover().is_err());
    assert_eq!(fs::read(&target).unwrap(), b"user edit");
    assert_eq!(fs::read(journal.original.path()).unwrap(), b"user edit");
    assert_eq!(fs::read(&part).unwrap(), b"replacement");
    assert!(journal.manifest.path().exists());
}

#[test]
fn concurrent_creation_never_gets_overwritten_by_publication_or_recovery() {
    let dir = TempDir::new().unwrap();
    let (target, part, mut journal) = prepare(dir.path());
    assert!(
        publish_journal(&journal, |stage| {
            if stage == Stage::Displaced {
                fs::write(&target, b"new user file")?;
            }
            Ok(())
        })
        .is_err()
    );
    assert!(journal.recover().is_err());
    assert_eq!(fs::read(&target).unwrap(), b"new user file");
    assert_eq!(fs::read(journal.original.path()).unwrap(), b"original");
    assert_eq!(fs::read(&part).unwrap(), b"replacement");
}

#[test]
fn changed_prepared_bytes_and_second_edit_are_preserved() {
    for change_prepared in [true, false] {
        let dir = TempDir::new().unwrap();
        let (target, part, mut journal) = prepare(dir.path());
        assert!(
            publish_journal(&journal, |stage| {
                if change_prepared && stage == Stage::Prepared {
                    fs::write(&part, b"changed preparation")?;
                }
                if !change_prepared && stage == Stage::Installed {
                    fs::write(&target, b"second user edit")?;
                }
                Ok(())
            })
            .is_err()
        );
        assert!(journal.recover().is_err());
        assert_eq!(fs::read(journal.original.path()).unwrap(), b"original");
        if change_prepared {
            assert_eq!(fs::read(&target).unwrap(), b"original");
            assert_eq!(fs::read(&part).unwrap(), b"changed preparation");
        } else {
            assert_eq!(fs::read(&target).unwrap(), b"second user edit");
            assert_eq!(fs::read(&part).unwrap(), b"second user edit");
        }
    }
}

#[test]
fn recovery_refuses_active_journal_and_unknown_entries() {
    let dir = TempDir::new().unwrap();
    let (target, _, journal) = prepare(dir.path());
    assert!(recover_target(&target).is_err());
    let unknown = journal.directory.path().join("user-file");
    fs::write(&unknown, b"user bytes").unwrap();
    drop(journal);
    assert!(recover_target(&target).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"original");
    assert_eq!(fs::read(&unknown).unwrap(), b"user bytes");
}

#[test]
fn recovery_rejects_unknown_versions_and_path_traversal() {
    for target_name in [b"../photo.jpg".as_slice(), b"photo.jpg".as_slice()] {
        let dir = TempDir::new().unwrap();
        let (target, _, mut journal) = prepare(dir.path());
        journal.evidence.target = target_name.to_vec();
        if target_name == b"photo.jpg" {
            journal.evidence.version += 1;
        }
        fs::write(
            journal.manifest.path(),
            serde_json::to_vec(&journal.evidence).unwrap(),
        )
        .unwrap();
        drop(journal);
        assert!(recover_target(&target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
    }
}

#[test]
fn unsupported_rename_flags_exercise_the_production_publication_routes() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[repr(C)]
    struct SyscallData {
        number: i32,
        architecture: u32,
        instruction_pointer: u64,
        arguments: [u64; 6],
    }
    const FLAGS_ARGUMENT: usize = 4;
    const FLAGS_OFFSET: usize = std::mem::offset_of!(SyscallData, arguments)
        + FLAGS_ARGUMENT * std::mem::size_of::<u64>()
        + if cfg!(target_endian = "big") { 4 } else { 0 };
    let routes = [
        "sync_core::tests::contract_replace_requires_recorded_bytes_new_versions_replace_only_untouched_files",
        "sync_core::tests::confirmed_damage_is_replaced_by_the_next_sync_only_while_unchanged",
        #[cfg(feature = "xmp")]
        "download::metadata::sidecar::tests::write_sidecar_creates_xmp_file_next_to_media",
        #[cfg(feature = "xmp")]
        "download::metadata::sidecar::tests::write_sidecar_is_atomic_rewrite",
    ];
    for error in [libc::EOPNOTSUPP, libc::EINVAL, libc::ENOSYS] {
        for route in &routes {
            let dir = TempDir::new().unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([route, "--exact", "--test-threads=1"]);
            command.env("TMPDIR", dir.path());
            // SAFETY: pre_exec calls only prctl, with stack-owned C filter
            // storage. The kernel copies it before returning; only the child
            // is filtered. No Rust locks, allocations, or other threads run
            // in this closure before exec.
            let install_filter = move || {
                let filter = [
                    libc::sock_filter {
                        code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                        jt: 0,
                        jf: 0,
                        k: std::mem::offset_of!(SyscallData, number) as u32,
                    },
                    libc::sock_filter {
                        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                        jt: 0,
                        jf: 3,
                        k: libc::SYS_renameat2 as u32,
                    },
                    libc::sock_filter {
                        code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                        jt: 0,
                        jf: 0,
                        k: FLAGS_OFFSET as u32,
                    },
                    libc::sock_filter {
                        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                        jt: 1,
                        jf: 0,
                        k: 0,
                    },
                    libc::sock_filter {
                        code: (libc::BPF_RET | libc::BPF_K) as u16,
                        jt: 0,
                        jf: 0,
                        k: libc::SECCOMP_RET_ERRNO | error as u32,
                    },
                    libc::sock_filter {
                        code: (libc::BPF_RET | libc::BPF_K) as u16,
                        jt: 0,
                        jf: 0,
                        k: libc::SECCOMP_RET_ALLOW,
                    },
                ];
                let program = libc::sock_fprog {
                    len: filter.len() as u16,
                    filter: filter.as_ptr().cast_mut(),
                };
                // SAFETY: PR_SET_NO_NEW_PRIVS takes only integer arguments.
                if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: the kernel copies the live C filter before returning.
                if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) }
                    != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            };
            // SAFETY: the closure calls only prctl before exec, with live stack
            // storage, and does not allocate or acquire process-local locks.
            unsafe {
                command.pre_exec(install_filter);
            }
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("running 1 test"),
                "errno={error}, route={route}, status={}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[cfg(feature = "xmp")]
#[test]
fn contract_temp_file_delete_requires_durable_ownership_of_inode_and_bytes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("prepared.xmp");
    fs::write(&path, b"owned output").unwrap();
    let expected = fingerprint_regular_file_blocking(&path).unwrap();
    let identity = crate::fs_util::file_identity(&fs::File::open(&path).unwrap()).unwrap();
    fs::write(&path, b"user changed bytes").unwrap();
    assert!(super::cleanup_prepared(&path, expected, identity).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"user changed bytes");
    fs::rename(&path, dir.path().join("saved-user-edit")).unwrap();
    fs::write(&path, b"owned output").unwrap();
    assert!(super::cleanup_prepared(&path, expected, identity).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"owned output");
    let current = crate::fs_util::file_identity(&fs::File::open(&path).unwrap()).unwrap();
    super::cleanup_prepared(&path, expected, current).unwrap();
    assert!(!path.exists());
    assert_eq!(
        fs::read(dir.path().join("saved-user-edit")).unwrap(),
        b"user changed bytes"
    );
}

#[test]
fn fallback_refuses_using_the_target_as_its_prepared_file() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("photo.jpg");
    fs::write(&target, b"user media").unwrap();
    let expected = fingerprint_regular_file_blocking(&target).unwrap();
    assert!(publish(&target, &target, expected, expected).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"user media");
    assert!(!directory_path(&target).unwrap().exists());
}

#[test]
fn cleanup_failure_retains_unknown_bytes_and_the_prepared_path_for_callers() {
    let dir = TempDir::new().unwrap();
    let (target, part, mut journal) = prepare(dir.path());
    publish_journal(&journal, |_| Ok(())).unwrap();
    fs::write(journal.original.path(), b"late user edit").unwrap();
    let error = super::finish_publication(&mut journal, &part).unwrap_err();
    let error = super::retain_recovery_error(error, &journal, &part);
    let disposition = crate::download::file::classify_conditional_publish_error(&error);
    assert!(disposition.target_changed);
    assert!(disposition.retained_paths.contains(&part));
    assert!(
        disposition
            .retained_paths
            .contains(&directory_path(&target).unwrap())
    );
    assert_eq!(fs::read(&target).unwrap(), b"replacement");
    assert_eq!(
        fs::read(journal.original.path()).unwrap(),
        b"late user edit"
    );
    assert_eq!(fs::read(&part).unwrap(), b"replacement");
    assert!(journal.manifest.path().exists());
}

#[test]
fn completed_transaction_releases_locks_even_with_a_fork_inherited_descriptor() {
    let dir = TempDir::new().unwrap();
    let (target, _, journal) = prepare(dir.path());
    let inherited_descriptor = journal.lock.as_ref().unwrap().try_clone().unwrap();
    drop(journal);
    recover_target(&target).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"original");
    assert!(!directory_path(&target).unwrap().exists());
    drop(inherited_descriptor);
}

#[test]
fn unsupported_filesystem_classification_preserves_the_error_chain() {
    for code in [libc::EOPNOTSUPP, libc::EINVAL, libc::ENOSYS] {
        let error = anyhow::Error::from(std::io::Error::from_raw_os_error(code))
            .context("outer publication context");
        let disposition = crate::download::file::classify_conditional_publish_error(&error);
        assert!(disposition.filesystem_unsupported);
        assert!(!disposition.target_changed);
        assert!(disposition.retained_paths.is_empty());
    }
    for code in [libc::EPERM, libc::ENOSPC, libc::EIO] {
        let error = std::io::Error::from_raw_os_error(code).into();
        assert!(
            !crate::download::file::classify_conditional_publish_error(&error)
                .filesystem_unsupported
        );
    }
}

// Dedicated subprocesses scope faults to disposable fixture paths. This models
// open-unlinked handles and a terminal errno; it does not emulate an NFS server.
pub(in crate::download::file) fn terminal_fixture_active(path: &std::path::Path) -> bool {
    std::env::var_os("KEI_TEST_TERMINAL_ROOT")
        .is_some_and(|root| path.starts_with(std::path::PathBuf::from(root)))
}

#[test]
fn terminal_cleanup_worker() {
    let Some(root) = std::env::var_os("KEI_TEST_TERMINAL_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let (target, part, mut journal) = prepare(&root);
    publish_journal(&journal, |_| Ok(())).unwrap();
    let mode = std::env::var("KEI_TEST_TERMINAL_MODE").unwrap();
    let result = super::finish_publication(&mut journal, &part);
    if ["unknown", "namespace", "target", "eio", "fsync"].contains(&mode.as_str()) {
        let error = super::retain_recovery_error(result.unwrap_err(), &journal, &part);
        let disposition = crate::download::file::classify_conditional_publish_error(&error);
        assert!(disposition.target_changed);
        assert!(disposition.retained_paths.contains(&part));
        assert!(part.exists());
        if mode == "unknown" {
            assert_eq!(
                fs::read(journal.directory.path().join("user-file")).unwrap(),
                b"user bytes"
            );
        }
        if mode == "namespace" {
            assert!(journal.directory.path().with_extension("saved").is_dir());
        }
        if mode == "target" {
            assert_eq!(fs::read(&target).unwrap(), b"late target edit");
        }
        if mode == "fsync" {
            assert!(journal.directory.path().exists());
        }
        return;
    }
    result.unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"replacement");
    assert!(!part.exists());
    drop(journal);
    let restart = root.join("restart");
    fs::create_dir(&restart).unwrap();
    let (restarted_target, restarted_part, restarted_journal) = prepare(&restart);
    publish_journal(&restarted_journal, |_| Ok(())).unwrap();
    drop(restarted_journal);
    recover_target(&restarted_target).unwrap();
    assert_eq!(fs::read(&restarted_target).unwrap(), b"replacement");
    // Recovery has no ownership evidence for this unrelated prepared path.
    assert_eq!(fs::read(&restarted_part).unwrap(), b"replacement");
    // Empty-manifest initialization must also close both read and lock handles.
    let empty = root.join("empty.jpg");
    let directory = directory_path(&empty).unwrap();
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("manifest.json"), b"").unwrap();
    recover_target(&empty).unwrap();
    if std::env::var("KEI_TEST_TERMINAL_MODE").as_deref() == Ok("descriptors") {
        assert!(!directory.exists());
    }
}

pub(super) fn terminal_sync_fault(path: &crate::fs_util::ConfinedPath) -> anyhow::Result<()> {
    if terminal_fixture_active(path.path())
        && std::env::var("KEI_TEST_TERMINAL_MODE").as_deref() == Ok("fsync")
    {
        return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
    }
    Ok(())
}
