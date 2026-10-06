use super::{DESTINATIONS, destination_digest, lock_download_destination, mutex_for_key};
use sha2::Digest;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn cancelled_and_aborted_waiters_cannot_split_or_leak_destination_owner() {
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("a.jpg");
    let key: super::DestinationKey = destination_digest(&destination).unwrap().finalize().into();
    let owner = lock_download_destination(&destination, &CancellationToken::new())
        .await
        .unwrap();
    let original = mutex_for_key(key).unwrap();
    for index in 0..100 {
        let token = CancellationToken::new();
        let waiter = lock_download_destination(&destination, &token);
        tokio::pin!(waiter);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), waiter.as_mut())
                .await
                .is_err()
        );
        if index % 2 == 0 {
            token.cancel();
            assert!(waiter.await.is_err());
        }
        assert!(std::sync::Arc::ptr_eq(
            &original,
            &mutex_for_key(key).unwrap()
        ));
    }
    drop(original);
    drop(owner);
    assert!(
        !DESTINATIONS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .contains_key(&key)
    );
    let token = CancellationToken::new();
    token.cancel();
    assert!(
        lock_download_destination(&destination, &token)
            .await
            .is_err()
    );
    assert!(
        !DESTINATIONS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .contains_key(&key)
    );
}

#[tokio::test]
async fn many_completed_destinations_leave_no_dead_registry_entries() {
    let dir = tempfile::tempdir().unwrap();
    let mut keys: Vec<super::DestinationKey> = Vec::new();
    for index in 0..1000 {
        let path = dir.path().join(format!("{index}.jpg"));
        keys.push(destination_digest(&path).unwrap().finalize().into());
        let guard = lock_download_destination(&path, &CancellationToken::new())
            .await
            .unwrap();
        drop(guard);
    }
    let registry = DESTINATIONS.get().unwrap().lock().unwrap();
    assert!(keys.iter().all(|key| !registry.contains_key(key)));
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "coordination creates no permanent files"
    );
}

#[tokio::test]
async fn last_waiter_cancellation_prunes_registry_after_owner_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jpg");
    let key: super::DestinationKey = destination_digest(&path).unwrap().finalize().into();
    let owner = lock_download_destination(&path, &CancellationToken::new())
        .await
        .unwrap();
    let token = CancellationToken::new();
    let waiter = lock_download_destination(&path, &token);
    tokio::pin!(waiter);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1), waiter.as_mut())
            .await
            .is_err()
    );
    // Owner completion wakes the queued waiter. Cancelling before its next
    // poll must release that future's strong owner and prune the final entry.
    drop(owner);
    token.cancel();
    assert!(waiter.await.is_err());
    assert!(
        !DESTINATIONS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .contains_key(&key)
    );
    assert!(
        lock_download_destination(&path, &CancellationToken::new())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn aborted_last_waiter_prunes_registry_after_owner_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jpg");
    let key: super::DestinationKey = destination_digest(&path).unwrap().finalize().into();
    let owner = lock_download_destination(&path, &CancellationToken::new())
        .await
        .unwrap();
    let started = std::sync::Arc::new(tokio::sync::Notify::new());
    let waiter_started = started.clone();
    let waiter = tokio::spawn(async move {
        let token = CancellationToken::new();
        let acquisition = lock_download_destination(&path, &token);
        tokio::pin!(acquisition);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), acquisition.as_mut())
                .await
                .is_err()
        );
        waiter_started.notify_one();
        acquisition.await
    });
    started.notified().await;
    // This current-thread test does not yield between waking and aborting the
    // last waiter. Arbitrary future destruction must prune its final Arc.
    drop(owner);
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(
        !DESTINATIONS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .contains_key(&key)
    );
}
