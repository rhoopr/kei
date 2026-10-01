use super::{
    Fingerprint, Journal, Manifest, Stage, confined, directory_path, publish, publish_journal,
    recover_target, recover_tree_blocking,
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
fn fallback_publishes_only_verified_replacement_and_removes_journal() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("photo.jpg");
    let part = dir.path().join("photo.part");
    fs::write(&target, b"original").unwrap();
    fs::write(&part, b"replacement").unwrap();
    publish(
        &part,
        &target,
        fingerprint_regular_file_blocking(&target).unwrap(),
        fingerprint_regular_file_blocking(&part).unwrap(),
    )
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"replacement");
    assert!(!part.exists());
    assert!(!directory_path(&target).unwrap().exists());
    recover_tree_blocking(dir.path()).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn recovery_restores_uncommitted_bytes_at_each_interruption_boundary() {
    for stage in [
        Stage::Prepared,
        Stage::Displaced,
        Stage::Installed,
        Stage::Committed,
    ] {
        let dir = TempDir::new().unwrap();
        let (target, part, journal) = prepare(dir.path());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            publish_journal(&journal, |current| {
                assert_ne!(current, stage, "simulated interruption");
                Ok(())
            })
            .unwrap();
        }));
        assert!(result.is_err());
        drop(journal);
        recover_tree_blocking(dir.path()).unwrap();
        assert_eq!(
            fs::read(&target).unwrap(),
            if stage == Stage::Committed {
                b"replacement".as_slice()
            } else {
                b"original".as_slice()
            }
        );
        assert_eq!(fs::read(&part).unwrap(), b"replacement");
        assert!(!directory_path(&target).unwrap().exists());
        // The unchanged next recovery must not repeat filesystem work.
        recover_tree_blocking(dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}

#[test]
fn concurrent_edit_before_displacement_is_restored_and_retained() {
    let dir = TempDir::new().unwrap();
    let (target, part, journal) = prepare(dir.path());
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
    let (target, part, journal) = prepare(dir.path());
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
        let (target, part, journal) = prepare(dir.path());
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
fn recovery_refuses_symlinked_journal_and_parent() {
    let dir = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let target = dir.path().join("photo.jpg");
    let name = directory_path(&target).unwrap();
    std::os::unix::fs::symlink(external.path(), &name).unwrap();
    assert!(recover_target(&target).is_err());
    assert!(recover_tree_blocking(dir.path()).is_err());
    assert_eq!(fs::read_dir(external.path()).unwrap().count(), 0);
    let parent = dir.path().join("linked-parent");
    std::os::unix::fs::symlink(external.path(), &parent).unwrap();
    recover_tree_blocking(&parent).unwrap();
    assert_eq!(fs::read_dir(external.path()).unwrap().count(), 0);
}

#[test]
fn empty_initialization_and_partial_cleanup_are_recoverable() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("photo.jpg");
    fs::create_dir(directory_path(&target).unwrap()).unwrap();
    recover_target(&target).unwrap();
    let (_, _, journal) = prepare(dir.path());
    publish_journal(&journal, |_| Ok(())).unwrap();
    super::unlink(&journal.original, 0).unwrap();
    drop(journal);
    recover_target(&target).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"replacement");
    assert!(!directory_path(&target).unwrap().exists());
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn production_sidecar_retry_recovers_interruption_and_reaches_steady_state() {
    use crate::download::metadata_rewrite::run_pending;
    use crate::download::pipeline::MetadataFlags;
    use crate::state::{SqliteStateDb, types::AssetMetadata};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    for stage in [Stage::Displaced, Stage::Installed, Stage::Committed] {
        let dir = TempDir::new().unwrap();
        let media = dir.path().join("photo.jpg");
        fs::write(&media, crate::test_helpers::minimal_jpeg_with_source_gps()).unwrap();
        let checksum = crate::download::file::compute_sha256(&media).await.unwrap();
        let db_path = dir.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("SIDECAR_RECOVERY")
            .filename("photo.jpg")
            .metadata(AssetMetadata {
                description: Some("Before".into()),
                metadata_hash: Some("before".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "SIDECAR_RECOVERY",
            "original",
            &media,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "SIDECAR_RECOVERY", "original")
            .await
            .unwrap();
        let initial = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".kei-tmp"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(initial.applied, 1);
        let target = media.with_file_name("photo.jpg.xmp");
        let part = dir.path().join("prepared.xmp");
        fs::copy(&target, &part).unwrap();
        let journal = Journal::create(
            &confined(&part).unwrap(),
            confined(&target).unwrap(),
            fingerprint_regular_file_blocking(&target).unwrap(),
            fingerprint_regular_file_blocking(&part).unwrap(),
        )
        .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            publish_journal(&journal, |current| {
                assert_ne!(current, stage, "simulated interruption");
                Ok(())
            })
            .unwrap();
        }));
        assert!(result.is_err());
        drop(journal);
        let updated = crate::test_helpers::TestAssetRecord::new("SIDECAR_RECOVERY")
            .filename("photo.jpg")
            .metadata(AssetMetadata {
                description: Some("After".into()),
                metadata_hash: Some("after".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&updated).await.unwrap();
        db.record_metadata_write_failure("PrimarySync", "SIDECAR_RECOVERY", "original")
            .await
            .unwrap();
        drop(db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let recovered = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".kei-tmp"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(recovered.applied, 1);
        assert_eq!(recovered.failed, 0);
        assert!(fs::read_to_string(&target).unwrap().contains("After"));
        assert!(!directory_path(&target).unwrap().exists());
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            crate::download::file::compute_sha256(&media).await.unwrap(),
            checksum
        );
        let steady = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".kei-tmp"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(steady.applied, 0);
        assert_eq!(steady.failed, 0);
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".kei-xmp-")
        }));
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
        "download::file::replacement::tests::approved_truncated_publish_replaces_only_expected_bytes",
        #[cfg(feature = "xmp")]
        "download::metadata::sidecar::tests::write_sidecar_creates_xmp_file_next_to_media",
        #[cfg(feature = "xmp")]
        "download::metadata::sidecar::tests::write_sidecar_is_atomic_rewrite",
        #[cfg(feature = "xmp")]
        "download::metadata_rewrite::queued::tests::drain_clears_previously_owned_sidecar_fields",
        #[cfg(feature = "xmp")]
        "download::metadata_rewrite::queued::tests::drain_establishes_a_checksum_baseline_when_none_was_recorded",
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
fn prepared_cleanup_requires_both_owned_inode_and_output_bytes() {
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
fn recovery_preserves_user_directories_that_only_share_the_prefix() {
    let dir = TempDir::new().unwrap();
    let user_directory = dir.path().join(".kei-replace-user-files");
    fs::create_dir(&user_directory).unwrap();
    fs::write(user_directory.join("manifest.json"), b"user metadata").unwrap();
    recover_tree_blocking(dir.path()).unwrap();
    assert_eq!(
        fs::read(user_directory.join("manifest.json")).unwrap(),
        b"user metadata"
    );
}

#[tokio::test]
async fn production_download_recovers_before_work_but_dry_run_does_not_mutate() {
    use crate::download::{DownloadConfig, DownloadControls, download_photos_with_sync};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let dir = TempDir::new().unwrap();
    let (target, _, journal) = prepare(dir.path());
    assert!(
        publish_journal(&journal, |stage| {
            anyhow::ensure!(stage != Stage::Displaced, "simulated interruption");
            Ok(())
        })
        .is_err()
    );
    drop(journal);
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    let config = Arc::new(config);
    download_photos_with_sync(
        &reqwest::Client::new(),
        &[],
        Arc::clone(&config),
        DownloadControls::dry_run_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!target.exists());
    assert!(directory_path(&target).unwrap().exists());
    download_photos_with_sync(
        &reqwest::Client::new(),
        &[],
        Arc::clone(&config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"original");
    assert!(!directory_path(&target).unwrap().exists());
    download_photos_with_sync(
        &reqwest::Client::new(),
        &[],
        config,
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"original");
}

#[tokio::test]
async fn production_download_keeps_checkpoint_when_recovery_is_unresolved() {
    use crate::download::{DownloadConfig, DownloadControls, download_photos_with_sync};
    use crate::state::{ScopedDbSyncToken, SqliteStateDb};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let dir = TempDir::new().unwrap();
    let (target, _, journal) = prepare(dir.path());
    assert!(
        publish_journal(&journal, |stage| {
            if stage == Stage::Displaced {
                fs::write(&target, b"concurrent user file")?;
            }
            Ok(())
        })
        .is_err()
    );
    drop(journal);
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let checkpoint = ScopedDbSyncToken {
        provider: "icloud".into(),
        account: "test-account".into(),
        shape_version: 1,
        scope_hash: "test-scope".into(),
        selected_zones_json: "[\"PrimarySync\"]".into(),
        scope_json: "{}".into(),
        token: "previous-checkpoint".into(),
    };
    db.upsert_scoped_db_sync_token(checkpoint).await.unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let result = download_photos_with_sync(
        &reqwest::Client::new(),
        &[],
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await;
    assert!(result.is_err());
    drop(db);
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    assert_eq!(
        db.get_scoped_db_sync_token("icloud", "test-account", 1, "test-scope")
            .await
            .unwrap()
            .unwrap()
            .token,
        "previous-checkpoint"
    );
    assert_eq!(fs::read(&target).unwrap(), b"concurrent user file");
    assert_eq!(
        fs::read(directory_path(&target).unwrap().join("original")).unwrap(),
        b"original"
    );
    assert!(directory_path(&target).unwrap().exists());
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
    let (target, part, journal) = prepare(dir.path());
    publish_journal(&journal, |_| Ok(())).unwrap();
    fs::write(journal.original.path(), b"late user edit").unwrap();
    let error = super::finish_publication(&journal, &part).unwrap_err();
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
    let inherited_descriptor = journal.lock.try_clone().unwrap();
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

#[test]
fn renamed_parent_retains_edited_original_and_all_journal_entries() {
    use std::io::Write;
    let dir = TempDir::new().unwrap();
    let parent = dir.path().join("photos");
    fs::create_dir(&parent).unwrap();
    let (target, _, journal) = prepare(&parent);
    let mut original = fs::OpenOptions::new().write(true).open(&target).unwrap();
    publish_journal(&journal, |_| Ok(())).unwrap();
    let moved = dir.path().join("moved");
    fs::rename(&parent, &moved).unwrap();
    original.write_all(b"user edit").unwrap();
    original.sync_all().unwrap();
    assert!(journal.finish().is_err());
    let moved_journal = moved.join(journal.directory.path().file_name().unwrap());
    assert_eq!(
        fs::read(moved_journal.join("original")).unwrap(),
        b"user edit"
    );
    assert_eq!(
        fs::read(moved_journal.join("replacement")).unwrap(),
        b"replacement"
    );
    assert!(moved_journal.join("manifest.json").exists());
    assert!(moved_journal.join("committed").exists());
    drop(journal);
    assert!(recover_tree_blocking(&moved).is_err());
    assert_eq!(
        fs::read(moved_journal.join("original")).unwrap(),
        b"user edit"
    );
}

#[test]
fn configured_root_alias_recovers_journals_without_following_descendant_links() {
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let (target, _, journal) = prepare(&real);
    assert!(
        publish_journal(&journal, |stage| {
            anyhow::ensure!(stage != Stage::Displaced, "interrupted");
            Ok(())
        })
        .is_err()
    );
    drop(journal);
    let external = TempDir::new().unwrap();
    let (external_target, _, external_journal) = prepare(external.path());
    std::os::unix::fs::symlink(external.path(), real.join("descendant")).unwrap();
    recover_tree_blocking(&alias).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"original");
    assert!(!directory_path(&target).unwrap().exists());
    assert!(directory_path(&external_target).unwrap().exists());
    recover_tree_blocking(&alias).unwrap();
    assert!(directory_path(&external_target).unwrap().exists());
    drop(external_journal);
}

#[test]
fn protected_legacy_journal_is_untouched_even_through_root_alias() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("shared");
    fs::create_dir(&root).unwrap();
    let (target, part, _journal) = prepare(&root);
    let journal_path = directory_path(&target).unwrap();
    let snapshot = || {
        let mut entries: Vec<_> = fs::read_dir(&journal_path)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_owned(),
                    fs::read(&path).unwrap(),
                )
            })
            .collect();
        entries.sort();
        entries
    };
    let before = snapshot();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    assert!(
        super::recover_tree_blocking_with_protection(&alias, std::slice::from_ref(&target),)
            .is_err()
    );
    assert_eq!(fs::read(&target).unwrap(), b"original");
    assert_eq!(fs::read(&part).unwrap(), b"replacement");
    assert_eq!(snapshot(), before);
    // Even an empty journal must not be removed by Journal::open.
    drop(_journal);
    fs::remove_dir_all(&journal_path).unwrap();
    fs::create_dir(&journal_path).unwrap();
    assert!(super::recover_tree_blocking_with_protection(&root, &[target]).is_err());
    assert!(journal_path.is_dir());
}
