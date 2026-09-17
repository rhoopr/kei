#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use reqwest::Client;
#[cfg(unix)]
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use crate::state::SqliteStateDb;

#[cfg(unix)]
use super::super::dispatch::download_photos_with_sync;
#[cfg(unix)]
use super::super::models::{DownloadControls, DownloadOutcome};
#[cfg(unix)]
use super::super::test_support::test_config;
use super::remove_owned_orphan_parts;
#[cfg(unix)]
use super::remove_owned_orphan_parts_with;

#[test]
fn owned_orphan_cleanup_removes_only_the_exact_recorded_path() {
    let dir = tempfile::tempdir().unwrap();
    let owned_path = dir.path().join("owned.kei-tmp");
    let bystander_path = dir.path().join("bystander.kei-tmp");
    std::fs::write(&owned_path, b"owned").unwrap();
    std::fs::write(&bystander_path, b"bystander").unwrap();
    let owned = [crate::state::OwnedTempFile {
        path: owned_path.clone(),
        claimed_at: 1,
    }];

    let cleanup = remove_owned_orphan_parts(dir.path(), &owned, i64::MAX / 2, i64::MAX / 2, 0);

    assert_eq!(cleanup.removed, 1);
    assert_eq!(cleanup.retire.as_slice(), std::slice::from_ref(&owned_path));
    assert!(!owned_path.exists());
    assert_eq!(std::fs::read(&bystander_path).unwrap(), b"bystander");
}

#[cfg(unix)]
#[test]
fn owned_orphan_cleanup_never_follows_a_directory_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().join("external.kei-tmp");
    std::fs::write(&outside_path, b"external").unwrap();
    let link = root.path().join("linked");
    symlink(outside.path(), &link).unwrap();
    let linked_path = link.join("external.kei-tmp");
    let owned = [crate::state::OwnedTempFile {
        path: linked_path.clone(),
        claimed_at: 1,
    }];

    let cleanup = remove_owned_orphan_parts(root.path(), &owned, i64::MAX / 2, i64::MAX / 2, 0);

    assert_eq!(cleanup.removed, 0);
    assert_eq!(cleanup.retire, [linked_path]);
    assert_eq!(std::fs::read(outside_path).unwrap(), b"external");
}

#[cfg(unix)]
#[test]
fn owned_orphan_cleanup_resists_ancestor_symlink_swap_before_remove() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let parent = root.path().join("parent");
    let moved_parent = root.path().join("moved-parent");
    std::fs::create_dir(&parent).unwrap();
    let owned_path = parent.join("owned.kei-tmp");
    let outside_path = outside.path().join("owned.kei-tmp");
    std::fs::write(&owned_path, b"owned").unwrap();
    std::fs::write(&outside_path, b"external").unwrap();
    let owned = [crate::state::OwnedTempFile {
        path: owned_path.clone(),
        claimed_at: 1,
    }];

    let cleanup =
        remove_owned_orphan_parts_with(root.path(), &owned, i64::MAX / 2, i64::MAX / 2, 0, |_| {
            std::fs::rename(&parent, &moved_parent).unwrap();
            symlink(outside.path(), &parent).unwrap();
        });

    assert_eq!(cleanup.removed, 1);
    assert_eq!(cleanup.retire.as_slice(), std::slice::from_ref(&owned_path));
    assert!(!moved_parent.join("owned.kei-tmp").exists());
    assert_eq!(std::fs::read(outside_path).unwrap(), b"external");
}

#[test]
fn owned_orphan_cleanup_spares_recently_touched_files() {
    let dir = tempfile::tempdir().unwrap();
    let part_path = dir.path().join("active.kei-tmp");
    std::fs::write(&part_path, b"active").unwrap();
    let owned = [crate::state::OwnedTempFile {
        path: part_path.clone(),
        claimed_at: 1,
    }];
    let real_now = chrono::Utc::now().timestamp();

    let cleanup = remove_owned_orphan_parts(
        dir.path(),
        &owned,
        real_now + 1_800,
        real_now + 3_600,
        90 * 60,
    );

    assert_eq!(cleanup.removed, 0);
    assert!(cleanup.retire.is_empty());
    assert_eq!(std::fs::read(part_path).unwrap(), b"active");
}

#[test]
fn owned_orphan_cleanup_retires_a_claim_when_the_path_changed_after_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let part_path = dir.path().join("changed.kei-tmp");
    std::fs::write(&part_path, b"replacement").unwrap();
    let owned = [crate::state::OwnedTempFile {
        path: part_path.clone(),
        claimed_at: -1,
    }];

    let cleanup = remove_owned_orphan_parts(dir.path(), &owned, 0, 0, 0);

    assert_eq!(cleanup.removed, 0);
    assert_eq!(cleanup.retire.as_slice(), std::slice::from_ref(&part_path));
    assert_eq!(std::fs::read(part_path).unwrap(), b"replacement");
}

#[cfg(unix)]
#[tokio::test]
async fn contract_temp_file_delete_requires_durable_ownership() {
    use std::fs::{File, FileTimes};
    use std::os::unix::fs::symlink;
    use std::time::{Duration as StdDuration, UNIX_EPOCH};

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let owned_path = root.path().join("owned.kei-tmp");
    let bystander_path = root.path().join("bystander.kei-tmp");
    let outside_path = outside.path().join("external.kei-tmp");
    std::fs::write(&owned_path, b"owned").unwrap();
    std::fs::write(&bystander_path, b"bystander").unwrap();
    std::fs::write(&outside_path, b"external").unwrap();
    symlink(outside.path(), root.path().join("linked")).unwrap();
    let old_time = UNIX_EPOCH + StdDuration::from_secs(1);
    for path in [&owned_path, &bystander_path, &outside_path] {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old_time))
            .unwrap();
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    db.claim_temp_file(&owned_path).await.unwrap();
    {
        let conn = db.acquire_lock("seed orphan cleanup").unwrap();
        conn.execute("UPDATE owned_temp_files SET claimed_at = 1", [])
            .unwrap();
    }
    let run_id = db.start_sync_run().await.unwrap();
    db.complete_sync_run(run_id, &crate::state::SyncRunStats::default())
        .await
        .unwrap();

    let mut config = test_config();
    config.directory = Arc::from(root.path());
    config.state_db = Some(db.clone());
    let result = download_photos_with_sync(
        &Client::new(),
        &[],
        Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert!(!owned_path.exists(), "owned stale path must be removed");
    assert_eq!(std::fs::read(&bystander_path).unwrap(), b"bystander");
    assert_eq!(std::fs::read(&outside_path).unwrap(), b"external");
    assert!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "cleanup must retire consumed ownership"
    );
}
