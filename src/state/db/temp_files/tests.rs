//! Tests moved from `state::db::tests`, with their original names and assertions.
use crate::state::db::SqliteStateDb;
use crate::state::db::test_support::test_dir;

#[tokio::test]
async fn owned_temp_file_claim_round_trips_and_retires_exact_path() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();
    let path = dir.path().join("asset.kei-tmp");
    let expected = crate::fs_util::absolute_lexical(&path).unwrap();

    db.claim_temp_file(&path).await.unwrap();
    db.claim_temp_file(&path).await.unwrap();

    let owned = db.get_owned_temp_files_before(i64::MAX).await.unwrap();
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].path, expected);
    assert!(owned[0].claimed_at > 0);
    assert_eq!(db.retire_temp_files(&[path]).await.unwrap(), 1);
    assert!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn owned_temp_file_path_round_trip_is_lossless_for_non_utf8_names() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();
    let path = dir
        .path()
        .join(OsString::from_vec(b"asset-\xff.kei-tmp".to_vec()));

    db.claim_temp_file(&path).await.unwrap();

    let owned = db.get_owned_temp_files_before(i64::MAX).await.unwrap();
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].path, path);
}
