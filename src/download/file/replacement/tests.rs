use tempfile::TempDir;

use super::super::fingerprint::{fingerprint_file, fingerprint_regular_file_blocking};
use super::super::publication::publish_part_to_final;
#[cfg(windows)]
use super::finish_windows_repair_exchange_blocking;
#[cfg(windows)]
use super::repair_backup_path;
use super::{
    FinalPublication, classify_conditional_publish_error, restore_repair_target_if_unchanged_with,
};

#[tokio::test]
async fn local_policy_publication_rejects_byte_and_mtime_races() {
    for mtime_only in [false, true] {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("copy.jpg");
        let part = dir.path().join("copy.part");
        std::fs::write(&target, b"local bytes").unwrap();
        std::fs::write(&part, b"cloud bytes").unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let fingerprint = fingerprint_file(&target).await.unwrap();
        let (expected, time) = if mtime_only {
            (
                b"local bytes".as_slice(),
                modified + std::time::Duration::from_secs(60),
            )
        } else {
            std::fs::write(&target, b"other bytes").unwrap();
            (b"other bytes".as_slice(), modified)
        };
        std::fs::File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_modified(time)
            .unwrap();
        let error = publish_part_to_final(
            &part,
            &target,
            FinalPublication::ReplaceLocal {
                fingerprint,
                modified: Some(modified),
            },
        )
        .await
        .unwrap_err();
        assert!(classify_conditional_publish_error(&error).target_changed);
        assert_eq!(std::fs::read(&target).unwrap(), expected);
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            time
        );
        assert_eq!(std::fs::read(&part).unwrap(), b"cloud bytes");
    }
}

#[test]
fn restore_rejects_second_edit_displaced_to_part_path() {
    let dir = TempDir::new().unwrap();
    let part = dir.path().join("photo.part");
    let final_path = dir.path().join("photo.jpg");
    let swap_path = dir.path().join("swap.tmp");
    std::fs::write(&part, b"original target").unwrap();
    std::fs::write(&final_path, b"prepared replacement").unwrap();
    let replacement = fingerprint_regular_file_blocking(&final_path).unwrap();

    let error = restore_repair_target_if_unchanged_with(
        &part,
        &final_path,
        &part,
        replacement,
        |part, final_path, _displaced| {
            std::fs::write(final_path, b"second concurrent edit")?;
            std::fs::rename(final_path, &swap_path)?;
            std::fs::rename(part, final_path)?;
            std::fs::rename(&swap_path, part)
        },
    )
    .unwrap_err();

    let disposition = classify_conditional_publish_error(&error);
    assert!(disposition.target_changed);
    assert!(disposition.retained_paths.contains(&part));
    assert_eq!(std::fs::read(&final_path).unwrap(), b"original target");
    assert_eq!(std::fs::read(&part).unwrap(), b"second concurrent edit");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_partial_replace_failure_restores_original_path() {
    use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

    let dir = TempDir::new().unwrap();
    let long_parent = dir
        .path()
        .join("nested".repeat(20))
        .join("nested".repeat(20));
    tokio::fs::create_dir_all(&long_parent).await.unwrap();
    for parent in [dir.path(), long_parent.as_path()] {
        let part = parent.join("photo.part");
        let final_path = parent.join("photo.jpg");
        let backup_path = repair_backup_path(&part);
        tokio::fs::write(&final_path, b"bad").await.unwrap();
        tokio::fs::write(&part, b"verified replacement")
            .await
            .unwrap();
        let expected = fingerprint_file(&final_path).await.unwrap();

        tokio::fs::rename(&final_path, &backup_path).await.unwrap();
        let error_code = i32::try_from(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2).unwrap();
        let error = finish_windows_repair_exchange_blocking(
            &final_path,
            &backup_path,
            expected,
            Err(std::io::Error::from_raw_os_error(error_code)),
        )
        .unwrap_err();

        let error_chain = format!("{error:#}");
        assert!(
            error_chain.contains("original target was restored"),
            "{error_chain}"
        );
        assert_eq!(tokio::fs::read(&final_path).await.unwrap(), b"bad");
        assert_eq!(
            tokio::fs::read(&part).await.unwrap(),
            b"verified replacement"
        );
        assert!(
            !backup_path.exists(),
            "restored backup path must be removed"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_long_path_replacement_retains_backup_and_guarded_restore() {
    use super::{exchange_repair_files_blocking, restore_repair_target_if_unchanged_blocking};
    use std::os::windows::ffi::OsStrExt;

    let dir = TempDir::new().unwrap();
    let parent = dir
        .path()
        .join("nested".repeat(20))
        .join("nested".repeat(20));
    std::fs::create_dir_all(&parent).unwrap();
    let part = parent.join("photo.part");
    let final_path = parent.join("photo.jpg");
    assert!(part.as_os_str().encode_wide().count() > 260);
    assert!(final_path.as_os_str().encode_wide().count() > 260);
    std::fs::write(&final_path, b"owned previous bytes").unwrap();
    std::fs::write(&part, b"verified replacement").unwrap();
    let original = fingerprint_regular_file_blocking(&final_path).unwrap();
    let replacement = fingerprint_regular_file_blocking(&part).unwrap();
    let backup = exchange_repair_files_blocking(&part, &final_path, original).unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), b"owned previous bytes");
    assert_eq!(std::fs::read(&final_path).unwrap(), b"verified replacement");
    assert!(!part.exists());
    // A late writer must survive while the previous bytes remain in the backup.
    std::fs::write(&final_path, b"late foreign writer").unwrap();
    assert!(
        !restore_repair_target_if_unchanged_blocking(&part, &final_path, &backup, replacement)
            .unwrap()
    );
    assert_eq!(std::fs::read(&final_path).unwrap(), b"late foreign writer");
    assert_eq!(std::fs::read(&backup).unwrap(), b"owned previous bytes");
    assert!(!part.exists());
    // Restoring the exact installed bytes permits the owner's guarded rollback.
    std::fs::write(&final_path, b"verified replacement").unwrap();
    assert!(
        restore_repair_target_if_unchanged_blocking(&part, &final_path, &backup, replacement)
            .unwrap()
    );
    assert_eq!(std::fs::read(&final_path).unwrap(), b"owned previous bytes");
    assert_eq!(std::fs::read(&part).unwrap(), b"verified replacement");
    assert!(!backup.exists());
}
