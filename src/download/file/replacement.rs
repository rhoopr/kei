//! Stable-input replacement, displaced-file verification, and restoration.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::fingerprint::{ExistingFileFingerprint, fingerprint_regular_file_blocking};
use super::platform::fsync_parent_dir_best_effort_blocking;
#[cfg(target_os = "macos")]
use super::platform::rename_exchange_blocking;
#[cfg(target_os = "linux")]
use super::platform::renameat2_exchange_blocking;
#[cfg(windows)]
use super::platform::{move_file_no_replace_blocking, replace_file_with_backup_blocking};

#[derive(Debug, thiserror::Error)]
pub(in crate::download) enum ConditionalPublishTargetChanged {
    #[error("Refusing to replace {path} because its bytes changed after write planning")]
    AfterPlanning { path: PathBuf },
    #[error(
        "Refusing to replace {path} because its bytes changed during conditional publication; the original target was restored"
    )]
    DuringPublication { path: PathBuf },
    #[error(
        "Refusing to replace {path} because the target changed or could not be verified after write planning"
    )]
    Unverifiable { path: PathBuf },
}

#[derive(Debug, thiserror::Error)]
#[error("Conditional publication must retain paths: {paths:?}")]
pub(super) struct ConditionalPublishMustRetainPaths {
    pub(super) paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::download) struct ConditionalPublishErrorDisposition {
    pub(in crate::download) target_changed: bool,
    #[cfg(target_os = "linux")]
    pub(in crate::download) filesystem_unsupported: bool,
    pub(in crate::download) retained_paths: Vec<PathBuf>,
}

pub(in crate::download) fn classify_conditional_publish_error(
    error: &anyhow::Error,
) -> ConditionalPublishErrorDisposition {
    let target_changed = error
        .downcast_ref::<ConditionalPublishTargetChanged>()
        .is_some()
        || error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|source| source.kind() == std::io::ErrorKind::NotFound);
    #[cfg(target_os = "linux")]
    let filesystem_unsupported = error
        .downcast_ref::<std::io::Error>()
        .is_some_and(super::platform::is_renameat2_unsupported);
    let retained_paths = error
        .downcast_ref::<ConditionalPublishMustRetainPaths>()
        .map(|marker| marker.paths.clone())
        .unwrap_or_default();
    ConditionalPublishErrorDisposition {
        target_changed,
        #[cfg(target_os = "linux")]
        filesystem_unsupported,
        retained_paths,
    }
}

/// Final-path behavior for a verified `.part` file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::download) enum FinalPublication {
    /// Preserve the global no-overwrite contract.
    #[default]
    NoReplace,
    /// Require this operation to create the entry; equal existing bytes do not
    /// establish writer ownership when historical claims cannot be indexed.
    CreateOnly,
    /// Replace only the exact truncated bytes authorized by retry planning.
    ReplaceTruncated(ExistingFileFingerprint),
}

pub(super) async fn replace_file_if_unchanged(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
) -> anyhow::Result<()> {
    let part_path = part_path.to_path_buf();
    let final_path = final_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        replace_file_if_unchanged_blocking(&part_path, &final_path, expected, None)
    })
    .await
    .map_err(std::io::Error::other)?
}

pub(in crate::download) fn publish_file_if_unchanged_blocking(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
    expected_replacement: ExistingFileFingerprint,
) -> anyhow::Result<()> {
    replace_file_if_unchanged_blocking(part_path, final_path, expected, Some(expected_replacement))
}

fn replace_file_if_unchanged_blocking(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
    expected_replacement: Option<ExistingFileFingerprint>,
) -> anyhow::Result<()> {
    let current = match fingerprint_regular_file_blocking(final_path) {
        Ok(current) => current,
        Err(error) => {
            return Err(error).context(ConditionalPublishTargetChanged::Unverifiable {
                path: final_path.to_path_buf(),
            });
        }
    };
    if current != expected {
        return Err(ConditionalPublishTargetChanged::AfterPlanning {
            path: final_path.to_path_buf(),
        }
        .into());
    }

    let replacement = match fingerprint_regular_file_blocking(part_path) {
        Ok(replacement) => replacement,
        Err(error) => {
            return Err(error)
                .with_context(|| {
                    format!("Could not verify replacement file {}", part_path.display())
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: vec![part_path.to_path_buf()],
                });
        }
    };
    if expected_replacement.is_some_and(|approved| approved != replacement) {
        return Err(anyhow::anyhow!(
            "Refusing to publish {} because its bytes changed after validation",
            part_path.display()
        ))
        .context(ConditionalPublishMustRetainPaths {
            paths: vec![part_path.to_path_buf()],
        });
    }
    // Pin the existing fallback lane only in dedicated subprocess tests.
    #[cfg(all(test, target_os = "linux"))]
    if crate::test_helpers::process_death_force_journal()
        || super::replacement_recovery::tests::terminal_fixture_active(part_path)
    {
        return super::replacement_recovery::publish(part_path, final_path, expected, replacement);
    }
    let displaced_path = match exchange_repair_files_blocking(part_path, final_path, expected) {
        Ok(displaced_path) => displaced_path,
        #[cfg(target_os = "linux")]
        Err(error) if classify_conditional_publish_error(&error).filesystem_unsupported => {
            return super::replacement_recovery::publish(
                part_path,
                final_path,
                expected,
                replacement,
            );
        }
        Err(error) if classify_conditional_publish_error(&error).target_changed => {
            return Err(error).context(ConditionalPublishTargetChanged::Unverifiable {
                path: final_path.to_path_buf(),
            });
        }
        Err(error) => {
            return match fingerprint_regular_file_blocking(final_path) {
                Ok(current) if current == expected => Err(error),
                Ok(_) | Err(_) => {
                    Err(error).context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                }
            };
        }
    };
    if let Some(expected_replacement) = expected_replacement {
        match fingerprint_regular_file_blocking(final_path) {
            Ok(installed) if installed == expected_replacement => {}
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "Published replacement at {} no longer matches the validated bytes",
                    final_path.display()
                ))
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: vec![displaced_path],
                });
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| {
                        format!(
                            "Could not verify the published replacement at {}",
                            final_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: vec![displaced_path],
                    });
            }
        }
    }
    let displaced = match fingerprint_regular_file_blocking(&displaced_path) {
        Ok(displaced) => displaced,
        Err(error) => {
            let restore_result = restore_repair_target_if_unchanged_blocking(
                part_path,
                final_path,
                &displaced_path,
                replacement,
            );
            return match restore_result {
                Ok(true) => Err(error).context(
                    ConditionalPublishTargetChanged::DuringPublication {
                        path: final_path.to_path_buf(),
                    },
                ),
                Ok(false) => Err(error)
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: vec![displaced_path],
                    }),
                Err(restore_error) => Err(restore_error)
                    .with_context(|| {
                        format!(
                            "Could not restore {} after displaced-target verification failed: {error:#}",
                            final_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    }),
            };
        }
    };
    if displaced != expected {
        let restore_result = restore_repair_target_if_unchanged_blocking(
            part_path,
            final_path,
            &displaced_path,
            replacement,
        );
        return match restore_result {
            Ok(true) => Err(ConditionalPublishTargetChanged::DuringPublication {
                path: final_path.to_path_buf(),
            }
            .into()),
            Ok(false) => Err(anyhow::anyhow!(
                "Refusing to replace {} because its bytes changed during conditional publication",
                final_path.display()
            ))
            .context(ConditionalPublishTargetChanged::Unverifiable {
                path: final_path.to_path_buf(),
            })
            .context(ConditionalPublishMustRetainPaths {
                paths: vec![displaced_path],
            }),
            Err(error) => Err(error)
                .with_context(|| {
                    format!(
                        "Could not restore {} after its bytes changed during conditional publication",
                        final_path.display()
                    )
                })
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                }),
        };
    }

    if let Err(error) = std::fs::remove_file(&displaced_path) {
        tracing::warn!(target: "kei::download::file",
            path = %displaced_path.display(),
            %error,
            "Failed to remove displaced file after verified replacement"
        );
    }
    fsync_parent_dir_best_effort_blocking(final_path);
    Ok(())
}

fn restore_repair_target_if_unchanged_blocking(
    part_path: &Path,
    final_path: &Path,
    displaced_path: &Path,
    replacement: ExistingFileFingerprint,
) -> anyhow::Result<bool> {
    restore_repair_target_if_unchanged_with(
        part_path,
        final_path,
        displaced_path,
        replacement,
        restore_exchanged_repair_files_blocking,
    )
}

fn restore_repair_target_if_unchanged_with(
    part_path: &Path,
    final_path: &Path,
    displaced_path: &Path,
    replacement: ExistingFileFingerprint,
    restore: impl FnOnce(&Path, &Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<bool> {
    if fingerprint_regular_file_blocking(final_path).ok() == Some(replacement) {
        if let Err(error) = restore(part_path, final_path, displaced_path) {
            return Err(error)
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: retained_restore_paths(part_path, displaced_path),
                });
        }
        match fingerprint_regular_file_blocking(part_path) {
            Ok(displaced_replacement) if displaced_replacement == replacement => {}
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "Restoring {} displaced different bytes to {}",
                    final_path.display(),
                    part_path.display()
                ))
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: retained_restore_paths(part_path, displaced_path),
                });
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| {
                        format!(
                            "Could not verify the entry displaced while restoring {}",
                            final_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: retained_restore_paths(part_path, displaced_path),
                    });
            }
        }
        return Ok(true);
    }
    Ok(false)
}

fn retained_restore_paths(part_path: &Path, displaced_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![part_path.to_path_buf()];
    if displaced_path != part_path {
        paths.push(displaced_path.to_path_buf());
    }
    paths
}

#[cfg(target_os = "linux")]
fn exchange_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    renameat2_exchange_blocking(part_path, final_path)?;
    Ok(part_path.to_path_buf())
}

#[cfg(target_os = "linux")]
fn restore_exchanged_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _displaced_path: &Path,
) -> std::io::Result<()> {
    renameat2_exchange_blocking(part_path, final_path)
}

#[cfg(target_os = "macos")]
fn exchange_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    rename_exchange_blocking(part_path, final_path)?;
    Ok(part_path.to_path_buf())
}

#[cfg(target_os = "macos")]
fn restore_exchanged_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _displaced_path: &Path,
) -> std::io::Result<()> {
    rename_exchange_blocking(part_path, final_path)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn exchange_repair_files_blocking(
    _part_path: &Path,
    _final_path: &Path,
    _expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic truncated-file replacement is unsupported on this platform",
    )
    .into())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn restore_exchanged_repair_files_blocking(
    _part_path: &Path,
    _final_path: &Path,
    _displaced_path: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic truncated-file replacement is unsupported on this platform",
    ))
}

#[cfg(windows)]
fn repair_backup_path(part_path: &Path) -> PathBuf {
    let mut name = part_path.as_os_str().to_os_string();
    name.push(".repair-backup");
    PathBuf::from(name)
}

#[cfg(windows)]
fn exchange_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    let backup_path = repair_backup_path(part_path);
    if backup_path.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("repair backup already exists at {}", backup_path.display()),
        )
        .into());
    }
    let replace_result = replace_file_with_backup_blocking(final_path, part_path, &backup_path);
    finish_windows_repair_exchange_blocking(final_path, &backup_path, expected, replace_result)
}

#[cfg(windows)]
fn finish_windows_repair_exchange_blocking(
    final_path: &Path,
    backup_path: &Path,
    expected: ExistingFileFingerprint,
    result: std::io::Result<()>,
) -> anyhow::Result<PathBuf> {
    use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

    match result {
        Ok(()) => Ok(backup_path.to_path_buf()),
        Err(source)
            if source
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
                == Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2) =>
        {
            let source_message = source.to_string();
            let displaced = match fingerprint_regular_file_blocking(backup_path) {
                Ok(displaced) => displaced,
                Err(error) => {
                    return Err(error)
                        .with_context(|| {
                            format!(
                                "Could not verify the partially displaced repair target; it remains at {}",
                                backup_path.display()
                            )
                        })
                        .context(ConditionalPublishMustRetainPaths {
                            paths: vec![backup_path.to_path_buf()],
                        });
                }
            };
            if displaced != expected {
                return Err(ConditionalPublishTargetChanged::DuringPublication {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: vec![backup_path.to_path_buf()],
                });
            }

            if let Err(error) = move_file_no_replace_blocking(backup_path, final_path) {
                return Err(error)
                    .with_context(|| {
                        format!(
                            "Could not restore {}; the original bytes remain at {}",
                            final_path.display(),
                            backup_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: vec![backup_path.to_path_buf()],
                    });
            }
            fsync_parent_dir_best_effort_blocking(final_path);
            Err(source).with_context(|| {
                format!(
                    "Windows file replacement partially failed after moving {}; the original target was restored: {source_message}",
                    final_path.display()
                )
            })
        }
        Err(source) => Err(source.into()),
    }
}

#[cfg(windows)]
fn restore_exchanged_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    displaced_path: &Path,
) -> std::io::Result<()> {
    if part_path.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("repair part path changed at {}", part_path.display()),
        ));
    }
    replace_file_with_backup_blocking(final_path, displaced_path, part_path)
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix, feature = "xmp"))]
thread_local! {
    static SIDECAR_CLEANUP_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Sidecar publication retains the same no-follow parents used for preparation.
#[cfg(feature = "xmp")]
pub(in crate::download) fn publish_sidecar_confined(
    part: &crate::fs_util::ConfinedPath,
    target: &crate::fs_util::ConfinedPath,
    expected: Option<ExistingFileFingerprint>,
    replacement: ExistingFileFingerprint,
    identity: crate::fs_util::FileIdentity,
) -> anyhow::Result<()> {
    fn fingerprint(
        path: &crate::fs_util::ConfinedPath,
    ) -> anyhow::Result<Option<(ExistingFileFingerprint, crate::fs_util::FileIdentity)>> {
        let Some(mut file) = path.open_optional_regular()? else {
            return Ok(None);
        };
        let identity = crate::fs_util::file_identity(&file)?;
        let result =
            super::fingerprint::fingerprint_open_file_snapshot_blocking(&mut file, path.path())?
                .fingerprint;
        path.validate_identity(identity)?;
        Ok(Some((result, identity)))
    }
    part.validate_identity(identity)?;
    anyhow::ensure!(
        fingerprint(part)? == Some((replacement, identity)),
        "Prepared sidecar changed; retaining output"
    );
    let current = fingerprint(target)?;
    if current.map(|(bytes, _)| bytes) != expected {
        anyhow::bail!(if expected.is_none() {
            "Sidecar appeared after preparation; retaining output"
        } else {
            "Sidecar bytes changed after preparation; retaining output"
        });
    }
    if let Some(expected) = expected {
        let original_identity = current.context("Existing sidecar disappeared")?.1;
        #[cfg(unix)]
        {
            #[cfg(all(test, target_os = "linux"))]
            let forced = crate::test_helpers::process_death_force_journal();
            #[cfg(not(all(test, target_os = "linux")))]
            let forced = false;
            let exchange = if forced {
                Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP))
            } else {
                super::platform::exchange_layout_confined(part, target)
            };
            #[cfg(target_os = "linux")]
            if exchange
                .as_ref()
                .err()
                .is_some_and(super::platform::is_renameat2_unsupported)
            {
                super::replacement_recovery::publish_confined(part, target, expected, replacement)?;
                target.sync_parent()?;
                return Ok(());
            }
            exchange?;
            let displaced = fingerprint(part);
            let installed = fingerprint(target);
            if displaced.as_ref().ok().copied().flatten() != Some((expected, original_identity))
                || installed.as_ref().ok().copied().flatten() != Some((replacement, identity))
            {
                if target.validate_identity(identity).is_ok()
                    && installed.as_ref().ok().copied().flatten() == Some((replacement, identity))
                {
                    super::platform::exchange_layout_confined(part, target)?;
                }
                return Err(anyhow::anyhow!(
                    "Sidecar changed during publication; retaining displaced bytes"
                ))
                .context(ConditionalPublishMustRetainPaths {
                    paths: vec![part.path().to_owned()],
                });
            }
            #[cfg(all(test, unix, feature = "xmp"))]
            SIDECAR_CLEANUP_HOOK.with(|hook| {
                if let Some(hook) = hook.borrow_mut().take() {
                    hook();
                }
            });
            anyhow::ensure!(
                fingerprint(part)? == Some((expected, original_identity)),
                "Displaced sidecar changed before cleanup; retaining bytes"
            );
            part.remove_regular(original_identity)?;
        }
        #[cfg(windows)]
        {
            let _part_pin = part.pin_identity(identity)?;
            let _target_pin = target.pin_identity(original_identity)?;
            publish_file_if_unchanged_blocking(part.path(), target.path(), expected, replacement)?;
        }
    } else {
        match super::platform::publish_reconciliation_part_blocking(part, target)? {
            super::platform::PublishResult::Published => {}
            super::platform::PublishResult::DestinationExists => {
                anyhow::bail!("Sidecar appeared after preparation; retaining output")
            }
        }
    }
    target.validate_identity(identity)?;
    anyhow::ensure!(
        fingerprint(target)? == Some((replacement, identity)),
        "Published sidecar changed before completion"
    );
    if let Some(file) = part.open_optional_regular()?
        && crate::fs_util::file_identity(&file)? == identity
        && fingerprint(part)? == Some((replacement, identity))
    {
        part.remove_regular(identity)?;
    }
    target.sync_parent()?;
    Ok(())
}
