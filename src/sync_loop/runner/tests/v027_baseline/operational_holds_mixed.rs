//! Current-delta mixed hold reachability, without a health-policy substitute.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use rusqlite::OpenFlags;
use wiremock::{Request, ResponseTemplate};

use super::operational_holds::{Fixture, reported_health};
use super::{MEDIA, ZONE, baseline_owner};
use crate::state::SparseIdentityStore;
use crate::sync_cycle::LibraryHoldKind;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Admission {
    root: String,
    specification_hash: String,
    pass: String,
    child: String,
    state_id: String,
    manifest_hash: String,
    version: String,
    path: String,
    checksum: String,
    size: i64,
    admitted: bool,
    verified: bool,
    admission: String,
    reason: String,
    admission_hash: Option<String>,
    attempts: i64,
    deadline: i64,
}

fn admissions(path: &std::path::Path) -> Result<Vec<Admission>, rusqlite::Error> {
    let db = rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = db.prepare(
        "SELECT d.generation,g.specification_hash,d.pass_key,d.child,w.asset_id,\
         d.manifest_hash,w.version_size,w.path,w.checksum,w.size_bytes,w.admitted,\
         w.verified_media,d.admission,d.reason,d.admission_hash,d.attempts,d.next_retry_at \
         FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation \
         JOIN provider_active_destinations w ON w.generation=d.generation \
         AND w.pass_key=d.pass_key AND w.child=d.child \
         WHERE d.child LIKE '%op-progress%' ORDER BY d.generation,d.pass_key,d.child,w.path",
    )?;
    statement
        .query_map([], |row| {
            Ok(Admission {
                root: row.get(0)?,
                specification_hash: row.get(1)?,
                pass: row.get(2)?,
                child: row.get(3)?,
                state_id: row.get(4)?,
                manifest_hash: row.get(5)?,
                version: row.get(6)?,
                path: row.get(7)?,
                checksum: row.get(8)?,
                size: row.get(9)?,
                admitted: row.get(10)?,
                verified: row.get(11)?,
                admission: row.get(12)?,
                reason: row.get(13)?,
                admission_hash: row.get(14)?,
                attempts: row.get(15)?,
                deadline: row.get(16)?,
            })
        })?
        .collect()
}

async fn mixed_case() -> Option<(crate::health::HealthStatus, serde_json::Value)> {
    let observations = Arc::new(Mutex::new(Vec::new()));
    let callback_observations = Arc::clone(&observations);
    let fixture = Fixture::with_progress(move |database| {
        move |_request: &Request| {
            let observation = admissions(&database).map_err(|error| error.to_string());
            callback_observations.lock().unwrap().push(observation);
            ResponseTemplate::new(500)
        }
    })
    .await?;

    let baseline = fixture.run(0, false, false).await;
    assert_eq!(baseline.stats.downloaded, 1, "{baseline:?}");
    assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
    assert!(fixture.db.sparse_identities(ZONE).await.unwrap().is_empty());
    let epoch_before = {
        let conn = fixture.db.acquire_lock("mixed prework epoch").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(epoch_before.state, "complete");
    assert_eq!(
        fixture
            .db
            .get_metadata(&fixture.key)
            .await
            .unwrap()
            .as_deref(),
        Some("op-hold-after-baseline")
    );
    let inventory_before = fixture.inventory_queries.load(Ordering::SeqCst);
    let held = fixture.run(1, true, false).await;
    assert!(held.stats.failed > 0, "{held:?}");
    assert_eq!(held.stats.state_write_failures, 0, "{held:?}");
    assert_eq!(held.stats.enumeration_errors, 0, "{held:?}");
    assert!(!held.session_expired && !held.stats.interrupted, "{held:?}");
    assert!(!held.can_advance_database_checkpoint());
    assert!(
        held.holds
            .iter()
            .any(|hold| hold.source == ZONE && hold.kind == LibraryHoldKind::IdentityUnresolved)
    );
    let source = held
        .sources
        .iter()
        .find(|source| source.zone_name == ZONE)
        .unwrap();
    assert!(source.reached);
    let sparse = source.sparse.as_ref().unwrap();
    assert!(
        sparse.completed_delta_replay,
        "actual current delta must reach EOF"
    );
    fixture
        .db
        .attest_current_sparse(&sparse.witness)
        .await
        .unwrap();
    let sparse_rows = fixture.db.sparse_identities(ZONE).await.unwrap();
    assert_eq!(sparse_rows.len(), 1);
    assert_eq!(sparse_rows[0].source.as_str(), "asset-op-hold-child");
    assert_eq!(
        fixture.inventory_queries.load(Ordering::SeqCst),
        inventory_before
    );
    let captured = observations.lock().unwrap().clone();
    assert!(!captured.is_empty(), "actual CDN failure must be reached");
    let before = captured[0].as_ref().expect("read at actual CDN request");
    assert_eq!(
        before.len(),
        1,
        "one admitted owner at dispatch: {before:?}"
    );
    let before = &before[0];
    assert_eq!(before.admission, "admitted");
    assert!(before.admitted && !before.verified);
    assert_eq!(before.child, "asset-op-progress");
    assert_eq!(before.size, MEDIA.len() as i64);
    let native_path: crate::state::db::provider_selection::SelectionPath =
        serde_json::from_str(&before.path).unwrap();
    assert_eq!(native_path.to_path(), fixture.root.join("progress.JPG"));
    assert!(before.admission_hash.is_some());
    let after = admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap();
    let after = after
        .iter()
        .find(|row| {
            row.root == before.root
                && row.specification_hash == before.specification_hash
                && row.pass == before.pass
                && row.child == before.child
                && row.state_id == before.state_id
                && row.manifest_hash == before.manifest_hash
                && row.version == before.version
                && row.path == before.path
                && row.checksum == before.checksum
                && row.size == before.size
        })
        .expect("exact admitted destination survives failed transfer");
    assert_eq!(after.reason, "download_failed");
    assert!(after.admitted && !after.verified);
    assert_ne!(after.admission_hash, before.admission_hash);
    assert_eq!(after.attempts, before.attempts + 1);
    assert_ne!(after.deadline, before.deadline);
    assert!(after.deadline > chrono::Utc::now().timestamp());
    let after = after.clone();
    let gets_before_reopen = captured.len();
    drop(captured);
    assert_eq!(
        fixture
            .db
            .get_metadata(&fixture.key)
            .await
            .unwrap()
            .as_deref(),
        Some("op-hold-after-baseline")
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
    assert!(!fixture.root.join("progress.JPG").exists());
    let epoch_after = {
        let conn = fixture.db.acquire_lock("mixed postwork epoch").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        (&epoch_after.state, &epoch_after.epoch, &epoch_after.attempt),
        (
            &epoch_before.state,
            &epoch_before.epoch,
            &epoch_before.attempt
        )
    );
    let (health, report) = reported_health(&fixture, &held, "mixed-report.json").await;

    let fixture = fixture.reopen().await;
    assert_eq!(
        admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap(),
        vec![after.clone()]
    );
    let reopened_sparse = fixture.db.sparse_identities(ZONE).await.unwrap();
    assert_eq!(reopened_sparse.len(), 1);
    assert_eq!(reopened_sparse[0].generation, sparse_rows[0].generation);
    assert_eq!(reopened_sparse[0].original, sparse_rows[0].original);
    assert_eq!(reopened_sparse[0].observed, sparse_rows[0].observed);
    let identity = fixture
        .db
        .selection_retry_identity(
            baseline_owner(),
            after.root.clone(),
            after.pass.clone(),
            after.child.clone(),
        )
        .await
        .unwrap()
        .expect("reopened state owner validates the retained destination");
    assert_eq!(identity.specification_hash, after.specification_hash);
    assert_eq!(identity.state_id, after.state_id);
    assert_eq!(identity.manifest_hash, after.manifest_hash);
    assert_eq!(Some(identity.admission_hash), after.admission_hash);
    assert!(identity.destinations.iter().any(|destination| {
        destination.version_size == after.version
            && destination.path.to_path() == fixture.root.join("progress.JPG")
            && destination.checksum == after.checksum
            && destination.size == after.size as u64
    }));
    assert_eq!(
        fixture
            .db
            .get_metadata(&fixture.key)
            .await
            .unwrap()
            .as_deref(),
        Some("op-hold-after-baseline")
    );
    let requests_before_quiet = fixture._server.received_requests().await.unwrap().len();
    let quiet = fixture.run(2, true, false).await;
    assert_eq!(
        fixture.inventory_queries.load(Ordering::SeqCst),
        inventory_before
    );
    assert_eq!(
        observations.lock().unwrap().len(),
        gets_before_reopen,
        "future retry must not GET early"
    );
    assert_eq!(
        fixture._server.received_requests().await.unwrap().len(),
        requests_before_quiet,
        "quiet reopen must not make any CDN request before the deadline"
    );
    assert_eq!(quiet.stats.enumeration_errors, 0, "{quiet:?}");
    assert_eq!(
        admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap(),
        vec![after]
    );
    assert_eq!(
        std::fs::read(fixture.root.join("baseline.JPG")).unwrap(),
        MEDIA
    );
    assert_eq!(
        fixture
            .db
            .get_metadata(&fixture.key)
            .await
            .unwrap()
            .as_deref(),
        Some("op-hold-after-baseline")
    );
    let quiet_sparse = fixture.db.sparse_identities(ZONE).await.unwrap();
    assert_eq!(quiet_sparse.len(), 1);
    assert_eq!(quiet_sparse[0].source, sparse_rows[0].source);
    assert_eq!(quiet_sparse[0].original, sparse_rows[0].original);
    assert!(quiet_sparse[0].generation >= sparse_rows[0].generation);
    let reopened_epoch = {
        let conn = fixture.db.acquire_lock("mixed reopened epoch").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        (
            &reopened_epoch.state,
            &reopened_epoch.epoch,
            &reopened_epoch.attempt
        ),
        (
            &epoch_before.state,
            &epoch_before.epoch,
            &epoch_before.attempt
        )
    );
    Some((health, report))
}

#[tokio::test]
async fn current_delta_sparse_hold_and_transfer_failure_reach_admitted_backoff() {
    let _ = Box::pin(mixed_case()).await;
}

#[tokio::test]
#[ignore = "mixed transfer health policy and committed successor are not implemented"]
async fn current_delta_sparse_hold_and_transfer_failure_should_advance_health() {
    let Some((health, report)) = Box::pin(mixed_case()).await else {
        return;
    };
    assert_eq!(report["status"], "partial_failure");
    assert!(
        health.last_success_at.is_some(),
        "desired mixed hold should advance operational health after its owner seam is implemented"
    );
}
