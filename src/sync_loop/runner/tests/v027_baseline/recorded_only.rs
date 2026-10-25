use super::{baseline_contract, pair};
use crate::icloud::photos::PhotoLibrary;
use crate::state::SqliteStateDb;
use serde_json::json;

tokio::task_local! { static ENABLED: bool; }

pub(super) fn enabled() -> bool {
    ENABLED.try_with(|value| *value).unwrap_or(false)
}

pub(super) async fn seed(library: &PhotoLibrary, db: &SqliteStateDb) {
    if !enabled() {
        return;
    }
    let album = library.all();
    let (capture, scope, zone) = album.catalog_work_scope().unwrap().unwrap();
    let records = pair(
        "old-only-unadmitted",
        "obsolete.JPG",
        "https://invalid.example/never",
        9,
    );
    let raw = serde_json::to_vec(&json!({"zones":[{"zoneID":zone,"records":records,
        "syncToken":"legacy-source-successor","moreComing":false}]}))
    .unwrap();
    capture
        .capture(
            crate::icloud::photos::catalog_observed_page(raw, &scope, "legacy-source").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        db.acquire_lock("verify real projected legacy source")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_catalog_records WHERE kind='asset'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
}

pub(super) fn assert_unadmitted(db: &SqliteStateDb) {
    if !enabled() {
        return;
    }
    let conn = db.acquire_lock("legacy source remains inert").unwrap();
    for table in [
        "provider_work_receipts",
        "provider_work_obligations",
        "provider_work_retries",
    ] {
        assert_eq!(
            conn.query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM provider_catalog_records WHERE kind='asset'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn v027_old_unadmitted_catalog_baseline_then_quiet_reopens_and_config_change() {
    ENABLED
        .scope(
            true,
            baseline_contract("private", false, false, false, false, false, false),
        )
        .await;
}

#[tokio::test]
async fn v027_inert_old_catalog_preserves_actual_failed_transfer_quiet_retry() {
    ENABLED
        .scope(
            true,
            baseline_contract("private", false, false, true, false, false, false),
        )
        .await;
}
