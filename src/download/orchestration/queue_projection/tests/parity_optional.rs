use super::{CURRENT, Fixture, OLD, owner, records};
use crate::download::orchestration::models::DownloadStore;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::SqliteStateDb;
use crate::state::db::provider_selection::{
    MAX_SELECTION_BYTES, SelectionDecision, SelectionDestination, SelectionManifest,
    SelectionOutcome, SelectionPath, SelectionSource,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn parity_rows(f: &Fixture) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = f.db.acquire_lock("historical parity snapshot").unwrap();
    let mut result = Vec::new();
    for table in [
        "provider_selection_generations",
        "provider_selection_sources",
        "provider_selection_decisions",
        "provider_selection_destinations",
    ] {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let count = stmt.column_count();
        result.extend(
            stmt.query_map([], |row| {
                (0..count)
                    .map(|i| row.get(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap(),
        );
    }
    result
}

#[tokio::test]
async fn v027_current_admission_does_not_require_new_parity_shadow_writes() {
    let mut f = Fixture::new().await;
    f.capture(
        records("m", OLD, "historical observation"),
        "retained-source",
    )
    .await;
    let plan = f.plan().await;
    let record = &plan.records[0];
    let manifest = SelectionManifest {
        scope: plan.scope.clone(),
        zone: plan.zone.clone(),
        config_hash: plan.config_hash.clone(),
        profile: serde_json::json!({"format":1,"coverage":"confirmed_sources_only","selection":"private-library-wide-without-exclusions","pass":"unfiled"}),
        sources: vec![SelectionSource {
            page_id: plan.source.page.id.0,
            ordinal: plan.source.ordinal,
            body_hash: plan.source.page.observation_hash.clone(),
        }],
        decisions: vec![SelectionDecision {
            pass_key: "unfiled".into(),
            child: record.id.to_string(),
            master: plan.master.clone(),
            confirmation: plan.confirmation.clone(),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations: vec![SelectionDestination {
                version_size: record.version_size.as_str().into(),
                path: SelectionPath::from_path(&f.config.directory.join("historical-parity.JPG")),
                checksum: record.checksum.to_string(),
                size: record.size_bytes,
                metadata_hash: record.metadata.metadata_hash.clone().unwrap(),
            }],
        }],
    };
    let old_id =
        f.db.capture_selection_shadow(owner(), manifest.clone(), MAX_SELECTION_BYTES)
            .await
            .unwrap();
    let old_parity = parity_rows(&f);
    assert!(!old_parity.is_empty());
    *f.session.records.lock().unwrap() = records("m", CURRENT, "new current confirmation");
    f.db.acquire_lock("reject only new parity writes").unwrap().execute_batch(
        "CREATE TRIGGER reject_new_parity BEFORE INSERT ON provider_selection_generations BEGIN SELECT RAISE(ABORT,'new parity capture is not normal admission'); END;"
    ).unwrap();
    // The real scheduler and admission owner, not a direct projection shortcut.
    f.cycle()
        .await
        .expect("current work must not require duplicate parity shadow");
    assert_eq!(f.count("assets"), 1);
    assert_eq!(f.count("provider_work_obligations"), 1);
    assert_eq!(f.count("provider_work_receipts"), 1);
    assert!(f.queue()[0].contains(CURRENT));
    assert!(f.queue()[0].contains("new current confirmation"));
    let source: Vec<u8> =
        f.db.acquire_lock("original source retained")
            .unwrap()
            .query_row("SELECT body FROM provider_shadow_pages", [], |r| r.get(0))
            .unwrap();
    assert!(String::from_utf8(source).unwrap().contains(OLD));
    let confirmation: Vec<u8> =
        f.db.acquire_lock("current confirmation retained")
            .unwrap()
            .query_row("SELECT confirmation FROM provider_work_receipts", [], |r| {
                r.get(0)
            })
            .unwrap();
    let confirmation = String::from_utf8(confirmation).unwrap();
    assert!(confirmation.contains("futureExact") && confirmation.contains("1.2300e+30"));
    let queue = f.queue();
    let calls = f.session.calls.load(Ordering::SeqCst);
    for _ in 0..2 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        f.db = db;
        f.capture = ShadowCapture::new(f.db.clone(), owner(), "com");
        f.pass
            .album
            .set_shadow_capture(f.capture.clone(), Arc::from("private"));
        f.config.state_db = Some(f.db.clone() as Arc<dyn DownloadStore>);
        f.cycle().await.unwrap();
        assert!(
            parity_rows(&f) == old_parity,
            "historical parity rows changed"
        );
        assert_eq!(
            f.db.replay_selection_shadow(owner(), old_id.clone())
                .await
                .unwrap(),
            manifest
        );
        assert_eq!(f.queue(), queue);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        f.preserved().await;
    }
    f.capture(records("n", OLD, "new unresolved source"), "new-unresolved")
        .await;
    *f.session.records.lock().unwrap() = records("n", CURRENT, "must not admit");
    *f.session.corrupt.lock().unwrap() = Some("pair");
    f.cycle().await.unwrap();
    assert_eq!(
        f.count("assets"),
        1,
        "invalid current master-child pair entered queue"
    );
    assert_eq!(f.queue(), queue);
    assert!(
        f.count("provider_work_retries") > 0,
        "unresolved source lost independent retry"
    );
    assert!(parity_rows(&f) == old_parity);
    f.preserved().await;
}
