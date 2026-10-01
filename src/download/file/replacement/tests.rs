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
async fn approved_truncated_publish_replaces_only_expected_bytes() {
    let dir = TempDir::new().unwrap();
    let part = dir.path().join("photo.part");
    let final_path = dir.path().join("photo.jpg");
    tokio::fs::write(&final_path, b"bad").await.unwrap();
    tokio::fs::write(&part, b"verified replacement")
        .await
        .unwrap();
    let expected = fingerprint_file(&final_path).await.unwrap();

    publish_part_to_final(
        &part,
        &final_path,
        FinalPublication::ReplaceTruncated(expected),
    )
    .await
    .unwrap();

    assert!(
        !part.exists(),
        "displaced truncated bytes should be removed"
    );
    assert_eq!(
        tokio::fs::read(&final_path).await.unwrap(),
        b"verified replacement"
    );
}

#[tokio::test]
async fn approved_truncated_publish_refuses_changed_target() {
    let dir = TempDir::new().unwrap();
    let part = dir.path().join("photo.part");
    let final_path = dir.path().join("photo.jpg");
    tokio::fs::write(&final_path, b"bad").await.unwrap();
    let expected = fingerprint_file(&final_path).await.unwrap();
    tokio::fs::write(&final_path, b"user replacement")
        .await
        .unwrap();
    tokio::fs::write(&part, b"verified replacement")
        .await
        .unwrap();

    let error = publish_part_to_final(
        &part,
        &final_path,
        FinalPublication::ReplaceTruncated(expected),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("bytes changed"), "{error}");
    assert_eq!(
        tokio::fs::read(&final_path).await.unwrap(),
        b"user replacement"
    );
    assert_eq!(
        tokio::fs::read(&part).await.unwrap(),
        b"verified replacement"
    );
}

#[tokio::test]
async fn approved_truncated_publish_classifies_deleted_target_as_changed() {
    let dir = TempDir::new().unwrap();
    let part = dir.path().join("photo.part");
    let final_path = dir.path().join("photo.jpg");
    tokio::fs::write(&final_path, b"bad").await.unwrap();
    let expected = fingerprint_file(&final_path).await.unwrap();
    tokio::fs::remove_file(&final_path).await.unwrap();
    tokio::fs::write(&part, b"verified replacement")
        .await
        .unwrap();

    let error = publish_part_to_final(
        &part,
        &final_path,
        FinalPublication::ReplaceTruncated(expected),
    )
    .await
    .unwrap_err();

    assert!(classify_conditional_publish_error(&error).target_changed);
    assert!(!final_path.exists());
    assert_eq!(
        tokio::fs::read(&part).await.unwrap(),
        b"verified replacement"
    );
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
    let part = dir.path().join("photo.part");
    let final_path = dir.path().join("photo.jpg");
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

#[cfg(unix)]
#[tokio::test]
async fn approved_truncated_publish_refuses_symlink_target() {
    use std::os::unix::fs::symlink;

    let dir = TempDir::new().unwrap();
    let underlying = dir.path().join("underlying.jpg");
    let final_path = dir.path().join("photo.jpg");
    let part = dir.path().join("photo.part");
    tokio::fs::write(&underlying, b"bad").await.unwrap();
    symlink(&underlying, &final_path).unwrap();
    tokio::fs::write(&part, b"verified replacement")
        .await
        .unwrap();
    let expected = fingerprint_file(&final_path).await.unwrap();

    let error = publish_part_to_final(
        &part,
        &final_path,
        FinalPublication::ReplaceTruncated(expected),
    )
    .await
    .unwrap_err();

    let error_chain = format!("{error:#}");
    assert!(error_chain.contains("regular file"), "{error_chain}");
    assert!(classify_conditional_publish_error(&error).target_changed);
    assert_eq!(tokio::fs::read_link(&final_path).await.unwrap(), underlying);
    assert_eq!(
        tokio::fs::read(&part).await.unwrap(),
        b"verified replacement"
    );
}
