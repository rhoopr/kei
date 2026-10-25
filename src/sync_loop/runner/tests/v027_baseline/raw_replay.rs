//! Old orphan response bytes are inert; explicit retry dependencies still fail closed.
use super::{ZONE, baseline_contract, baseline_owner, pair, zone};
use crate::icloud::photos::PhotoLibrary;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::SqliteStateDb;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

tokio::task_local! { static ENABLED: bool; }

async fn captured(capture: &ShadowCapture) -> (i64, i64) {
    let scope = capture.scope("private", &zone()).unwrap();
    let raw = serde_json::to_vec(&json!({"zones":[{"zoneID":zone(),
        "records":pair("old-raw", "old.JPG", "https://invalid.example/never", 29),
        "syncToken":"old-successor","moreComing":false}]}))
    .unwrap();
    capture
        .capture(crate::icloud::photos::catalog_observed_page(raw, &scope, "old-request").unwrap())
        .await
        .unwrap();
    capture
        .db
        .acquire_lock("retained raw fixture identity")
        .unwrap()
        .query_row(
            "SELECT page_id,ordinal FROM provider_catalog_records WHERE kind='asset'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn damage(db: &SqliteStateDb, id: i64) {
    let conn = db
        .acquire_lock("synthetic malformed historical response")
        .unwrap();
    conn.execute("DELETE FROM provider_catalog_pages WHERE page_id=?1", [id])
        .unwrap();
    conn.execute(
        "UPDATE provider_shadow_pages SET body=?2 WHERE id=?1",
        rusqlite::params![id, b"malformed retained response".as_slice()],
    )
    .unwrap();
}

pub(super) async fn seed(library: &PhotoLibrary, db: &SqliteStateDb) {
    if !ENABLED.try_with(|enabled| *enabled).unwrap_or(false) {
        return;
    }
    let capture = library.shadow_capture.as_ref().unwrap();
    let (id, _) = captured(capture).await;
    damage(db, id);
}

pub(super) async fn startup(library: &PhotoLibrary) {
    if !ENABLED.try_with(|enabled| *enabled).unwrap_or(false) {
        return;
    }
    let capture = library.shadow_capture.as_ref().unwrap();
    capture
        .replay_catalog(&CancellationToken::new())
        .await
        .expect("orphan raw history must not veto normal sync");
    let conn = capture.db.acquire_lock("orphan remains inert").unwrap();
    let retained: (Vec<u8>, i64) = conn.query_row(
        "SELECT body,(SELECT count(*) FROM provider_catalog_pages c WHERE c.page_id=p.id) FROM provider_shadow_pages p",
        [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(retained, (b"malformed retained response".to_vec(), 0));
}

#[tokio::test]
async fn v027_orphan_malformed_raw_history_does_not_block_baseline_or_quiet_cycles() {
    ENABLED
        .scope(
            true,
            baseline_contract("private", false, false, false, false, false, false),
        )
        .await;
}

#[tokio::test]
async fn v027_dependent_malformed_raw_history_preserves_retry_and_cursor_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Arc::new(
        SqliteStateDb::open_owned(&path, &baseline_owner())
            .await
            .unwrap(),
    );
    let capture = ShadowCapture::new(db.clone(), baseline_owner(), "com");
    let (id, ordinal) = captured(&capture).await;
    let cursor = crate::sync_cycle::sync_token_key(ZONE);
    db.set_metadata(&cursor, "established-current-cursor")
        .await
        .unwrap();
    let source = db
        .catalog_source(
            baseline_owner(),
            crate::state::db::provider_inbox::CapturedPageId(id),
            16 * 1024 * 1024,
        )
        .await
        .unwrap();
    let admission = db
        .project_provider_work(
            baseline_owner(),
            crate::state::db::provider_work::WorkPlan {
                source: crate::state::db::provider_work::WorkSource {
                    page: source,
                    ordinal,
                },
                scope: capture.scope("private", &zone()).unwrap(),
                zone: zone(),
                config_hash: "a".repeat(64),
                confirmation: None,
                master: None,
                records: Vec::new(),
                reason: "current_lookup_unresolved",
            },
            crate::state::db::provider_work::MAX_WORK_BYTES,
        )
        .await
        .unwrap();
    assert_eq!(
        admission,
        crate::state::db::provider_work::WorkAdmission::Deferred
    );
    // A valid already-projected source remains available to its existing reader;
    // startup neither duplicates projection nor retires its retry.
    capture
        .replay_catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert!(
        db.catalog_source(
            baseline_owner(),
            crate::state::db::provider_inbox::CapturedPageId(id),
            16 * 1024 * 1024
        )
        .await
        .is_ok()
    );
    damage(&db, id);
    drop(capture);
    drop(db);
    for _ in 0..2 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&path, &baseline_owner())
                .await
                .unwrap(),
        );
        let capture = ShadowCapture::new(db.clone(), baseline_owner(), "com");
        let scope = capture.scope("private", &zone()).unwrap();
        let hash = "a".repeat(64);
        // Unselected scopes and changed configuration cannot inherit this
        // source's error or consume its independent retry obligation.
        for (selected_scope, selected_hash) in [
            (capture.scope("shared", &zone()).unwrap(), hash.clone()),
            (scope.clone(), "b".repeat(64)),
        ] {
            capture
                .replay_due_catalog(selected_scope, selected_hash, 64, &CancellationToken::new())
                .await
                .unwrap();
        }
        {
            let conn = db.acquire_lock("future retry remains independent").unwrap();
            conn.execute(
                "UPDATE provider_work_retries SET next_retry_at=?1",
                [chrono::Utc::now().timestamp() + 3600],
            )
            .unwrap();
        }
        capture
            .replay_due_catalog(scope.clone(), hash.clone(), 64, &CancellationToken::new())
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("make retained retry due").unwrap();
            conn.execute(
                "UPDATE provider_work_retries SET last_attempt_at=0,next_retry_at=1",
                [],
            )
            .unwrap();
        }
        assert!(
            capture
                .replay_due_catalog(scope, hash, 64, &CancellationToken::new())
                .await
                .is_err(),
            "only this selected scope's due evidence must fail its preflight"
        );
        assert_eq!(
            db.get_metadata(&cursor).await.unwrap().as_deref(),
            Some("established-current-cursor")
        );
        let conn = db
            .acquire_lock("failed dependent projection remains durable")
            .unwrap();
        let counts: (i64,i64,i64) = conn.query_row(
            "SELECT (SELECT count(*) FROM provider_work_retries),(SELECT count(*) FROM provider_shadow_pages),(SELECT count(*) FROM provider_catalog_pages)",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(counts, (1, 1, 0));
    }
}

mod two_library;
