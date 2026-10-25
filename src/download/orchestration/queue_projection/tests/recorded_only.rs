use super::{Fixture, OLD, controls, has_due_retained_work, owner, records};
use crate::state::db::provider_work::WorkAdmission;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn v027_bare_catalog_does_not_schedule_or_admit_new_work() {
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let before = f.session.calls.load(Ordering::SeqCst);
    for changed in [false, true, false] {
        f.config.folder_structure = if changed { "changed-template" } else { "none" }.into();
        assert!(
            !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
                .await
                .unwrap()
        );
        f.cycle().await.unwrap();
        for table in [
            "provider_work_receipts",
            "provider_work_obligations",
            "provider_work_retries",
        ] {
            assert_eq!(f.count(table), 0, "bare history cannot create {table}");
        }
    }
    assert_eq!(f.session.calls.load(Ordering::SeqCst), before);
    f.preserved().await;
}

#[tokio::test]
async fn v027_recorded_deferred_retry_keeps_original_config_and_backoff() {
    let mut f = Fixture::new().await;
    f.capture(records("m", OLD, "source"), "old").await;
    let mut plan = f.plan().await;
    plan.records.clear();
    plan.reason = "currently_filtered";
    assert_eq!(
        f.db.project_provider_work(owner(), plan, 512 * 1024 * 1024)
            .await
            .unwrap(),
        WorkAdmission::Deferred
    );
    let original = f.config.folder_structure.clone();
    let retry = || {
        f.db.acquire_lock("original retry binding").unwrap().query_row(
            "SELECT config_hash,attempts,last_attempt_at,next_retry_at FROM provider_work_retries",
            [], |r| Ok((r.get::<_,String>(0)?, r.get::<_,i64>(1)?, r.get::<_,i64>(2)?, r.get::<_,i64>(3)?))).unwrap()
    };
    let saved = retry();
    assert!(
        !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap()
    );
    f.config.folder_structure = "changed-template".into();
    f.cycle().await.unwrap();
    assert_eq!(retry(), saved);
    assert_eq!(f.count("provider_work_receipts"), 1);
    f.config.folder_structure = original;
    assert!(
        !has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap()
    );
    f.db.acquire_lock("make original retry due")
        .unwrap()
        .execute_batch("UPDATE provider_work_retries SET last_attempt_at=0,next_retry_at=1")
        .unwrap();
    assert!(
        has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap()
    );
    f.cycle().await.unwrap();
    assert_eq!(f.count("provider_work_retries"), 0);
    assert_eq!(f.count("provider_work_obligations"), 1);
    assert!(
        has_due_retained_work(std::slice::from_ref(&f.pass), &f.config, controls())
            .await
            .unwrap(),
        "admitted pending work still wakes its existing retry owner"
    );
    f.preserved().await;
}
