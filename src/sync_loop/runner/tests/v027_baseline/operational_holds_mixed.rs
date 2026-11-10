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

type PeerProvenance = (String, String, String, String, String, String, bool);

fn peer_provenance(db: &crate::state::SqliteStateDb) -> Vec<PeerProvenance> {
    let conn = db.acquire_lock("healthy peer provenance").unwrap();
    let mut statement = conn
        .prepare(
            "SELECT g.scope,d.library,d.child,d.master,d.asset_id,d.path,d.verified_media \
             FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation \
             WHERE d.library=?1 AND d.child='asset-op-healthy-peer' ORDER BY g.id,d.version_size,d.path",
        )
        .unwrap();
    let rows = statement
        .query_map([ZONE], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap();
    rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
}

fn preserved_pair_metadata(db: &crate::state::SqliteStateDb) -> Vec<String> {
    let conn = db
        .acquire_lock("snapshot existing source metadata")
        .unwrap();
    [
        "SELECT json_group_array(json_array(id,version_size,filename,created_at,added_at,metadata_hash,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size,is_deleted)) FROM (SELECT * FROM assets WHERE library=?1 AND id IN ('asset-op-baseline','asset-op-healthy-peer') ORDER BY id,version_size)",
        "SELECT json_group_array(json_array(id,version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size)) FROM (SELECT * FROM asset_metadata_paths WHERE library=?1 AND id IN ('asset-op-baseline','asset-op-healthy-peer') ORDER BY id,version_size,local_path)",
        "SELECT json_group_array(json_array(asset_record_name,master_record_name)) FROM (SELECT * FROM asset_master_mappings WHERE library=?1 AND asset_record_name IN ('asset-op-baseline','asset-op-healthy-peer') ORDER BY asset_record_name)",
    ].iter().map(|sql| conn.query_row(sql, [ZONE], |row| row.get(0)).unwrap()).collect()
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
    let mut held = fixture.run(1, true, false).await;
    assert!(held.stats.failed > 0, "{held:?}");
    assert_eq!(
        held.stats.downloaded, 1,
        "healthy peer must complete during the mixed cycle: {held:?}"
    );
    let healthy_peer = fixture.root.join("healthy-peer.JPG");
    assert_eq!(std::fs::read(&healthy_peer).unwrap(), MEDIA);
    let healthy_peer_mtime = std::fs::metadata(&healthy_peer)
        .unwrap()
        .modified()
        .unwrap();
    let peer_rows = peer_provenance(&fixture.db);
    assert!(
        peer_rows.iter().any(
            |(scope, library, child, master, state_id, path, verified)| {
                library == ZONE
                    && child == "asset-op-healthy-peer"
                    && master == "op-healthy-peer"
                    && state_id == "asset-op-healthy-peer"
                    && *verified
                    && crate::icloud::photos::PhotoLibrary::provider_scope_projection(scope)
                        == Some((
                            ZONE.to_owned(),
                            "_defaultOwner".to_owned(),
                            "private".to_owned(),
                        ))
                    && serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                        path,
                    )
                    .is_ok_and(|saved| saved.to_path() == healthy_peer)
            }
        ),
        "healthy peer must have its exact verified source and native destination: {peer_rows:?}"
    );
    assert_eq!(held.stats.state_write_failures, 0, "{held:?}");
    assert_eq!(held.stats.enumeration_errors, 0, "{held:?}");
    assert!(!held.session_expired && !held.stats.interrupted, "{held:?}");
    assert!(!held.can_advance_database_checkpoint());
    let partial = crate::sync_loop::runner::one_shot_partial_error(&held)
        .expect("mixed held work must retain a partial exit");
    assert_eq!(
        crate::classify_exit_error(&anyhow::Error::new(partial)).exit_code(),
        2
    );
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
    let journal_guards = fixture.finish_mixed_tail(&mut held).await;
    let (health, report) = reported_health(&fixture, &held, "mixed-report.json").await;
    drop(journal_guards);
    drop(held);

    let fixture = fixture.reopen().await;
    assert_eq!(
        peer_provenance(&fixture.db),
        peer_rows,
        "peer provenance survives reopen"
    );
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
    let mut quiet = fixture.run(2, true, false).await;
    let quiet_guards = fixture.finish_mixed_tail(&mut quiet).await;
    assert!(
        quiet_guards.is_some(),
        "reopen must account the prework future owner without a transient failure cause"
    );
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
        quiet.stats.downloaded, 0,
        "quiet reopen must not redownload the peer"
    );
    assert_eq!(std::fs::read(&healthy_peer).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&healthy_peer)
            .unwrap()
            .modified()
            .unwrap(),
        healthy_peer_mtime
    );
    assert_eq!(
        peer_provenance(&fixture.db),
        peer_rows,
        "peer provenance survives the quiet cycle"
    );
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
    drop(quiet_guards);
    Some((health, report))
}

#[tokio::test]
async fn current_delta_sparse_hold_and_transfer_failure_reach_admitted_backoff() {
    let _ = Box::pin(mixed_case()).await;
}

#[tokio::test]
async fn current_delta_sparse_hold_and_transfer_failure_should_advance_health() {
    let (health, report) = Box::pin(mixed_case())
        .await
        .expect("mixed operational fixture must execute");
    assert_eq!(report["status"], "partial_failure");
    assert!(
        health.last_success_at.is_some(),
        "desired mixed hold should advance operational health after its owner seam is implemented"
    );
}

async fn mixed_refusal_fixture() -> (Fixture, crate::sync_cycle::CycleResult) {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("mixed operational fixture must execute");
    let baseline = Box::pin(fixture.run(0, false, false)).await;
    assert!(baseline.can_advance_database_checkpoint());
    let held = Box::pin(fixture.run(1, true, false)).await;
    assert!(held.stats.failed > 0);
    assert!(
        held.holds.iter().any(|hold| {
            hold.source == ZONE && hold.kind == LibraryHoldKind::IdentityUnresolved
        })
    );
    (fixture, held)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn mixed_health_refuses_real_journal_before_and_after_tail_boundary() {
    for after_prepare in [false, true] {
        let (fixture, mut held) = mixed_refusal_fixture().await;
        // Exercise a stale sparse-only pre-tail attestation as well as the
        // mixed case's usual initial state.
        held.operational_safe_holds = true;
        let target = fixture.root.join("baseline.JPG");
        let part = fixture.root.join("journal-fixture.part");
        if !after_prepare {
            crate::download::file::seed_test_replacement_journal(
                &target,
                &part,
                MEDIA,
                b"synthetic replacement",
            )
            .unwrap();
        }
        let guards = fixture
            .finish_mixed_tail_with(&mut held, |prepared| {
                assert_eq!(prepared, after_prepare, "pre-tail owner observation");
                if after_prepare {
                    crate::download::file::seed_test_replacement_journal(
                        &target,
                        &part,
                        MEDIA,
                        b"synthetic replacement",
                    )
                    .unwrap();
                }
            })
            .await;
        assert!(guards.is_none(), "a real replacement journal vetoes health");
        assert!(!held.operational_safe_holds);
        let (health, report) = reported_health(&fixture, &held, "journal-refusal.json").await;
        assert!(health.last_success_at.is_none());
        assert_eq!(report["status"], "partial_failure");
        assert_eq!(std::fs::read(&target).unwrap(), MEDIA);
        assert_eq!(std::fs::read(&part).unwrap(), b"synthetic replacement");
    }
}

#[tokio::test]
async fn mixed_health_refuses_released_account_session_lock() {
    let (fixture, mut held) = mixed_refusal_fixture().await;
    fixture.release_account_lock().await;
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_none());
    let (health, report) = reported_health(&fixture, &held, "session-refusal.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
}

#[tokio::test]
async fn mixed_health_refuses_stale_successor_then_accepts_exact_owner() {
    let (fixture, mut held) = mixed_refusal_fixture().await;
    let before = admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap();
    let source = held
        .sources
        .iter_mut()
        .find(|source| source.zone_name == ZONE)
        .unwrap();
    assert_eq!(source.media_failures.len(), 1);
    let failure = source.media_failures.first_mut().unwrap();
    let committed = failure.successor.committed_admission_hash.clone();
    failure.successor.committed_admission_hash.push('0');

    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_none(), "stale successor must not grant health");
    let (health, report) = reported_health(&fixture, &held, "stale-successor.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(
        admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap(),
        before,
        "a refused proof must not change durable retry state"
    );

    held.sources
        .iter_mut()
        .find(|source| source.zone_name == ZONE)
        .unwrap()
        .media_failures
        .first_mut()
        .unwrap()
        .successor
        .committed_admission_hash = committed;
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_some(), "the unchanged exact owner remains usable");
    let (health, report) = reported_health(&fixture, &held, "exact-successor.json").await;
    assert!(health.last_success_at.is_some());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(
        std::fs::read(fixture.root.join("healthy-peer.JPG")).unwrap(),
        MEDIA
    );
}

#[tokio::test]
async fn mixed_health_refuses_wrong_provider_pin_then_accepts_real_owner() {
    let (fixture, mut held) = mixed_refusal_fixture().await;
    let source = held
        .sources
        .iter_mut()
        .find(|source| source.zone_name == ZONE)
        .unwrap();
    let prework = source.operational_prework.as_mut().unwrap();
    let original = prework.owner.clone();
    let other_login =
        serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"other-provider"}})).unwrap();
    prework.owner = crate::state::db::account::AccountOwner::authenticated(
        "baseline@example.invalid",
        "com",
        &other_login,
    )
    .unwrap();
    let before = admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap();
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_none());
    let (health, report) = reported_health(&fixture, &held, "wrong-pin.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(
        admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap(),
        before
    );

    held.sources
        .iter_mut()
        .find(|source| source.zone_name == ZONE)
        .unwrap()
        .operational_prework
        .as_mut()
        .unwrap()
        .owner = original;
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_some(), "the real account owner remains valid");
    let (health, report) = reported_health(&fixture, &held, "correct-pin.json").await;
    assert!(health.last_success_at.is_some());
    assert_eq!(report["status"], "partial_failure");
}

#[tokio::test]
async fn mixed_health_refuses_epoch_and_source_projection_drift() {
    let (fixture, mut held) = mixed_refusal_fixture().await;
    let original = held
        .sources
        .iter()
        .find(|source| source.zone_name == ZONE)
        .unwrap()
        .sparse
        .as_ref()
        .unwrap()
        .witness
        .clone();
    for epoch_drift in [true, false] {
        let witness = &mut held
            .sources
            .iter_mut()
            .find(|source| source.zone_name == ZONE)
            .unwrap()
            .sparse
            .as_mut()
            .unwrap()
            .witness;
        if epoch_drift {
            witness.bind_epoch("different-epoch", "different-attempt");
        } else {
            witness.bind_provider_scope_projection((
                ZONE.to_owned(),
                "different-owner".to_owned(),
                "private".to_owned(),
            ));
        }
        let guards = fixture.finish_mixed_tail(&mut held).await;
        assert!(guards.is_none());
        let (health, report) = reported_health(&fixture, &held, "source-drift.json").await;
        assert!(health.last_success_at.is_none());
        assert_eq!(report["status"], "partial_failure");
        held.sources
            .iter_mut()
            .find(|source| source.zone_name == ZONE)
            .unwrap()
            .sparse
            .as_mut()
            .unwrap()
            .witness = original.clone();
    }
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_some(), "the real epoch and source remain valid");
    let (health, _) = reported_health(&fixture, &held, "source-current.json").await;
    assert!(health.last_success_at.is_some());
}

#[tokio::test]
async fn mixed_health_refuses_owner_created_source_revalidation_debt() {
    let (fixture, mut held) = mixed_refusal_fixture().await;
    let scope = fixture
        .db
        .get_metadata(&format!("download_ledger_scope:{ZONE}"))
        .await
        .unwrap()
        .expect("actual source scope");
    let policy = fixture
        .db
        .get_metadata("enum_config_hash")
        .await
        .unwrap()
        .expect("actual enumeration policy");
    let boundary = fixture
        .db
        .get_metadata(&fixture.key)
        .await
        .unwrap()
        .expect("actual source checkpoint");
    fixture
        .db
        .seed_source_revalidation(
            baseline_owner(),
            ZONE.to_owned(),
            scope.clone(),
            policy.clone(),
            boundary.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let read_debt = || {
        let conn = fixture
            .db
            .acquire_lock("inspect source revalidation")
            .unwrap();
        let mut statement = conn
            .prepare(
                "SELECT r.id,r.phase,r.mode,d.id,d.next_retry_at FROM source_revalidation_rounds r \
                 JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library=?1 \
                 AND r.scope=?2 AND r.enum_hash=?3 AND r.boundary=?4 ORDER BY d.id",
            )
            .unwrap();
        statement
            .query_map(rusqlite::params![ZONE, scope, policy, boundary], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = read_debt();
    assert!(!before.is_empty(), "the source owner must create real debt");
    assert!(before.iter().all(|row| {
        row.1 == "observing"
            && row.2 == crate::state::db::source_revalidation::LEGACY_MODE
            && row.4 == 0
    }));
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_none());
    let (health, report) = reported_health(&fixture, &held, "source-debt-refusal.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(read_debt(), before, "refusal must retain source debt");
}

#[tokio::test]
async fn catalog_health_pair_normal_expiry_creates_active_debt_that_vetoes_mixed_health() {
    let fixture = Fixture::with_progress(|_| ResponseTemplate::new(500))
        .await
        .expect("mixed operational fixture must execute");
    let baseline = Box::pin(fixture.run(0, false, false)).await;
    assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
    let baseline_path = fixture.root.join("baseline.JPG");
    let baseline_mtime = std::fs::metadata(&baseline_path)
        .unwrap()
        .modified()
        .unwrap();

    // The ordinary rejected-cursor path installs and activates a real census.
    // The runner's separate catalog drain has not yet run.
    let expiry = Box::pin(fixture.run(3, false, false)).await;
    assert!(expiry.can_advance_database_checkpoint(), "{expiry:?}");
    let read_catalog = || {
        let conn = fixture.db.acquire_lock("read real catalog debt").unwrap();
        let mut query = conn
            .prepare(
                "SELECT r.id,r.scope,r.boundary,r.phase,r.mode,d.id,d.next_retry_at \
             FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id \
             WHERE r.library=?1 ORDER BY r.id,d.id",
            )
            .unwrap();
        query
            .query_map([ZONE], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let active = read_catalog();
    assert_eq!(active.len(), 1, "{active:?}");
    assert_eq!(
        (&*active[0].3, &*active[0].4, &*active[0].5),
        ("active", "catalog_v2", "asset-op-baseline")
    );
    let origin = crate::state::db::source_revalidation::CatalogOrigin::parse(&active[0].1).unwrap();
    let policy = fixture
        .db
        .get_metadata("enum_config_hash")
        .await
        .unwrap()
        .unwrap();
    let candidates = fixture
        .db
        .due_catalog_candidates(
            baseline_owner(),
            ZONE.to_owned(),
            origin.clone(),
            policy.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        candidates.len(),
        1,
        "current origin must authorize the actual due candidate"
    );
    let mut held = Box::pin(fixture.run(4, true, false)).await;
    assert_eq!(
        held.stats.failed, 1,
        "one actual transfer failure: {held:?}"
    );
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
    assert!(source.sparse.as_ref().unwrap().completed_delta_replay);
    assert_eq!(source.media_failures.len(), 1);
    fixture
        .db
        .attest_current_sparse(&source.sparse.as_ref().unwrap().witness)
        .await
        .unwrap();
    // This component slice does not run the catalog drain. In the full runner,
    // pre-tail accounting happens before that drain and must already refuse.
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(
        guards.is_none(),
        "active census debt must veto known sparse/media accounting"
    );
    let (health, report) = reported_health(&fixture, &held, "catalog-active-veto.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 1);
    assert_eq!(read_catalog(), active);
    assert_eq!(std::fs::read(&baseline_path).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&baseline_path)
            .unwrap()
            .modified()
            .unwrap(),
        baseline_mtime
    );
}

#[tokio::test]
async fn catalog_health_positive_recovers_real_sparse_identity() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests = attempts.clone();
    let recovered_transfer = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let response_ready = recovered_transfer.clone();
    let fixture = Fixture::with_progress(move |_| {
        move |_: &Request| {
            requests.fetch_add(1, Ordering::SeqCst);
            if response_ready.load(Ordering::SeqCst) {
                ResponseTemplate::new(200).set_body_bytes(MEDIA)
            } else {
                ResponseTemplate::new(500)
            }
        }
    })
    .await
    .expect("synthetic provider must execute");
    let baseline = Box::pin(fixture.run(0, false, false)).await;
    assert!(baseline.can_advance_database_checkpoint(), "{baseline:?}");
    let expiry = Box::pin(fixture.run(3, false, false)).await;
    assert!(expiry.can_advance_database_checkpoint(), "{expiry:?}");
    let old: (String, String, i64, String, String) = {
        let conn = fixture
            .db
            .acquire_lock("read active catalog obligation")
            .unwrap();
        conn.query_row("SELECT r.id,d.id,d.next_retry_at,r.scope,json_array(r.library,r.scope,r.enum_hash,r.boundary,r.high_id,r.after_id,r.mode) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library=?1 AND r.phase='active' AND r.mode='catalog_v2'", [ZONE], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap()
    };
    assert_eq!(old.1, "asset-op-baseline");
    let old_origin = crate::state::db::source_revalidation::CatalogOrigin::parse(&old.3).unwrap();
    let old_policy = fixture
        .db
        .get_metadata("enum_config_hash")
        .await
        .unwrap()
        .unwrap();
    let initial_due = fixture
        .db
        .due_catalog_candidates(
            baseline_owner(),
            ZONE.to_owned(),
            old_origin.clone(),
            old_policy,
        )
        .await
        .unwrap();
    assert!(
        initial_due
            .iter()
            .any(|candidate| candidate.round == old.0 && candidate.id == old.1)
    );
    let held = Box::pin(fixture.run(4, true, false)).await;
    assert_eq!(
        held.stats.failed, 1,
        "the first real transfer fails: {held:?}"
    );
    assert!(
        held.holds
            .iter()
            .any(|hold| hold.source == ZONE && hold.kind == LibraryHoldKind::IdentityUnresolved)
    );
    let held_sparse = fixture.db.sparse_identities(ZONE).await.unwrap();
    assert_eq!(held_sparse.len(), 1);
    assert_eq!(held_sparse[0].source.as_str(), "asset-op-hold-child");
    let held_source = held
        .sources
        .iter()
        .find(|source| source.zone_name == ZONE)
        .unwrap();
    assert_eq!(held_source.media_failures.len(), 1);
    let held_witness = held_source.sparse.as_ref().unwrap();
    assert!(held_witness.completed_delta_replay);
    fixture
        .db
        .attest_current_sparse(&held_witness.witness)
        .await
        .unwrap();
    let captured = expiry
        .sources
        .iter()
        .find(|source| source.zone_name == ZONE)
        .unwrap()
        .operational_prework
        .as_ref()
        .expect("pre-expiry source owner was captured");
    let census = {
        let conn = fixture
            .db
            .acquire_lock("inspect captured source census")
            .unwrap();
        captured.prework.census_evidence_for_test(&conn).unwrap()
    };
    assert_eq!(
        census,
        (true, true, true, false),
        "the actual source and plan are current but the census refuses"
    );
    assert!(
        captured
            .db
            .operational_accounting(
                captured.prework.clone(),
                vec![held_source.media_failures[0].successor.clone()],
                Some(held_witness.witness.clone()),
                false
            )
            .await
            .unwrap()
            .is_none()
    );
    let baseline_path = fixture.root.join("baseline.JPG");
    let peer_path = fixture.root.join("healthy-peer.JPG");
    let baseline_mtime = std::fs::metadata(&baseline_path)
        .unwrap()
        .modified()
        .unwrap();
    let peer_mtime = std::fs::metadata(&peer_path).unwrap().modified().unwrap();
    let peer_before = peer_provenance(&fixture.db);
    let metadata_before = preserved_pair_metadata(&fixture.db);
    // Observe the transaction before census marking can add this same item.
    // The TEMP table records facts and is not consulted by any owner.
    {
        let conn = fixture.db.acquire_lock("observe catalog transfer").unwrap();
        conn.execute_batch("CREATE TEMP TABLE pair_transfer_observation(old_round TEXT,new_round TEXT,item TEXT,old_deadline INTEGER,new_deadline INTEGER);
            CREATE TEMP TRIGGER pair_transfer_observer AFTER UPDATE OF phase ON main.source_revalidation_rounds
            WHEN OLD.phase='active' AND NEW.phase='abandoned'
            BEGIN INSERT INTO pair_transfer_observation
            SELECT OLD.id,r.id,d.id,d.next_retry_at,n.next_retry_at
            FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.library=OLD.library AND r.id<>OLD.id AND r.phase='marking'
            JOIN source_revalidation_debt n ON n.round=r.id AND n.id=d.id WHERE d.round=OLD.id; END;").unwrap();
        conn.execute_batch("CREATE TEMP TABLE peer_completion_observation(scope TEXT,library TEXT,child TEXT,master TEXT,state_id TEXT,path TEXT,verified INTEGER,receipt_id TEXT);
            CREATE TEMP TRIGGER peer_completion_observer AFTER DELETE ON main.provider_active_destinations
            WHEN OLD.child='asset-op-healthy-peer'
            BEGIN INSERT INTO peer_completion_observation
            SELECT g.scope,OLD.library,OLD.child,OLD.master,OLD.asset_id,OLD.path,OLD.verified_media,r.id
            FROM provider_active_generations g JOIN download_receipts r ON r.scope=g.scope AND r.library=OLD.library
            AND r.child=OLD.child AND r.master=OLD.master AND r.state_id=OLD.asset_id AND r.version_size=OLD.version_size
            AND r.provider_checksum=OLD.checksum AND r.provider_size=OLD.size_bytes AND r.native_path=OLD.path
            AND r.source_checksum=OLD.source_checksum AND r.local_checksum=OLD.local_checksum
            AND r.account_key=g.account_key AND r.provider_key=g.provider_key
            WHERE g.id=OLD.generation AND OLD.admitted=1 AND OLD.verified_media=1; END;").unwrap();
    }
    let (trace, _trace_guard) = crate::test_helpers::TracingCapture::install();
    let failed_attempts = attempts.load(Ordering::SeqCst);
    assert!(failed_attempts > 0);
    recovered_transfer.store(true, Ordering::SeqCst);
    let mut recovered = Box::pin(fixture.run(5, false, false)).await;
    let events = fixture.recovery_events.lock().unwrap().clone();
    assert!(
        recovered.can_advance_database_checkpoint(),
        "{recovered:?}; provider events: {events:?}"
    );
    assert_eq!(
        recovered.stats.failed, 0,
        "the separately observed retry returned 200"
    );
    assert!(
        fixture.db.sparse_identities(ZONE).await.unwrap().is_empty(),
        "source owner must retire the original sparse identity"
    );
    assert!(
        events
            .iter()
            .any(|event| event == "lookup:asset-op-hold-child" || event == "lookup:op-hold-child"),
        "actual recovery lookup: {events:?}"
    );
    assert!(events.iter().any(|event| event == "inventory"));
    assert!(events.iter().any(|event| event == "bridge"));
    assert!(
        attempts.load(Ordering::SeqCst) > failed_attempts,
        "the failed transfer was actually retried successfully"
    );
    assert_eq!(std::fs::read(fixture.root.join("hold.JPG")).unwrap(), MEDIA);
    assert_eq!(
        std::fs::read(fixture.root.join("progress.JPG")).unwrap(),
        MEDIA
    );
    assert_eq!(
        std::fs::read(fixture.root.join("healthy-peer.JPG")).unwrap(),
        MEDIA
    );
    let trace_events = trace.events();
    let new_id = trace_events
        .iter()
        .find(|event| event.message() == Some("test fresh catalog transaction returned"))
        .and_then(|event| event.field("nonce"))
        .expect("the owner transaction must return its origin");
    let (old_phase, copied_phase, copy): (String, String, (String, String, String, i64, i64)) = {
        let conn = fixture
            .db
            .acquire_lock("verify transferred catalog debt")
            .unwrap();
        let phases = conn.query_row("SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1), (SELECT phase FROM source_revalidation_rounds WHERE id=?2)", rusqlite::params![old.0,new_id], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        let copy = conn.query_row("SELECT old_round,new_round,item,old_deadline,new_deadline FROM pair_transfer_observation", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pair_transfer_observation",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        (phases.0, phases.1, copy)
    };
    assert_eq!(
        (old_phase.as_str(), copied_phase.as_str()),
        ("abandoned", "active")
    );
    assert_eq!(
        (&copy.0, copy.1.as_str(), &copy.2, copy.3, copy.4),
        (&old.0, new_id, &old.1, old.2, old.2)
    );
    let policy = fixture
        .db
        .get_metadata("enum_config_hash")
        .await
        .unwrap()
        .unwrap();
    let origin = {
        let conn = fixture
            .db
            .acquire_lock("read current catalog origin")
            .unwrap();
        let scope: String = conn
            .query_row(
                "SELECT scope FROM source_revalidation_rounds WHERE id=?1",
                [new_id],
                |row| row.get(0),
            )
            .unwrap();
        crate::state::db::source_revalidation::CatalogOrigin::parse(&scope).unwrap()
    };
    assert_eq!(
        (&origin.plan_digest, &origin.epoch, &origin.attempt),
        (
            &old_origin.plan_digest,
            &old_origin.epoch,
            &old_origin.attempt
        )
    );
    {
        let conn = fixture
            .db
            .acquire_lock("verify current source epoch and observation")
            .unwrap();
        let epoch = crate::state::db::sync_epoch::read_library(&conn, ZONE)
            .unwrap()
            .unwrap();
        assert_eq!(
            (&*epoch.state, &epoch.epoch, &epoch.attempt),
            ("complete", &origin.epoch, &origin.attempt)
        );
        let (token, round, boundary): (String,String,String) = conn.query_row(
            "SELECT (SELECT value FROM metadata WHERE key=?1), (SELECT json_extract(value,'$.revalidation_round') FROM metadata WHERE key=?2), (SELECT boundary FROM source_revalidation_rounds WHERE id=?3)",
            rusqlite::params![fixture.key,format!("observed_selection:v27:{ZONE}"),new_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        assert_eq!((token.as_str(), round.as_str()), ("pair-after-two", new_id));
        assert_eq!(
            boundary,
            crate::state::db::source_revalidation::CatalogOrigin::boundary("pair-fresh-two")
        );
    }
    let due = fixture
        .db
        .due_catalog_candidates(baseline_owner(), ZONE.to_owned(), origin, policy)
        .await
        .unwrap();
    assert!(
        due.iter().any(|candidate| candidate.round == new_id
            && candidate.id == old.1
            && candidate.master == "op-baseline"
            && candidate.child.as_deref() == Some("asset-op-baseline")),
        "actual ACTIVE source must authorize the copied child: {due:?}"
    );
    let events_before_drain = fixture.recovery_events.lock().unwrap().len();
    assert_eq!(
        fixture.drain_recovery_catalog(&mut recovered).await,
        0,
        "strict source drain must settle current debt"
    );
    let drain_events = fixture.recovery_events.lock().unwrap()[events_before_drain..].to_vec();
    assert!(
        drain_events
            .iter()
            .any(|event| event == "lookup:op-baseline")
            && drain_events
                .iter()
                .any(|event| event == "lookup:asset-op-baseline"),
        "strict lookup must request the exact pair: {drain_events:?}"
    );
    let (old_debt, new_debt): (i64, i64) = {
        let conn = fixture.db.acquire_lock("verify strict settlement").unwrap();
        conn.query_row("SELECT (SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2), (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?3)", rusqlite::params![old.0,old.1,new_id], |row| Ok((row.get(0)?,row.get(1)?))).unwrap()
    };
    assert_eq!((old_debt, new_debt), (old.2, 0));
    let old_history: String = fixture.db.acquire_lock("verify abandoned history").unwrap().query_row(
        "SELECT json_array(library,scope,enum_hash,boundary,high_id,after_id,mode) FROM source_revalidation_rounds WHERE id=?1 AND phase='abandoned'", [&old.0], |row| row.get(0),
    ).unwrap();
    assert_eq!(old_history, old.4);
    assert_eq!(preserved_pair_metadata(&fixture.db), metadata_before);
    assert_eq!(std::fs::read(&baseline_path).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&baseline_path)
            .unwrap()
            .modified()
            .unwrap(),
        baseline_mtime
    );
    assert_eq!(std::fs::read(&peer_path).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&peer_path).unwrap().modified().unwrap(),
        peer_mtime
    );
    // Completed generations may retire only after their exact source-bound
    // receipt is saved. Observe that handoff instead of requiring a transient
    // active-generation row to remain forever.
    assert!(peer_provenance(&fixture.db).is_empty());
    let (peer_handoff, receipt_id): (PeerProvenance, String) = {
        let conn = fixture
            .db
            .acquire_lock("verify peer completion handoff")
            .unwrap();
        conn.query_row("SELECT scope,library,child,master,state_id,path,verified,receipt_id FROM peer_completion_observation", [], |row| Ok(((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?),row.get(7)?))).unwrap()
    };
    assert_eq!(peer_before, vec![peer_handoff]);
    let receipt_retained: bool = fixture
        .db
        .acquire_lock("verify retained peer receipt")
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM download_receipts WHERE id=?1 AND length(proof_hash)=64)",
            [&receipt_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(receipt_retained);
    // A new, independently observed sparse child supplies the partial hold for
    // the health check. The original child above must remain resolved.
    let mut followup = Box::pin(fixture.run(6, true, false)).await;
    assert_eq!(
        followup.stats.failed, 0,
        "the follow-up has no new failed transfer: {followup:?}"
    );
    assert!(
        followup
            .holds
            .iter()
            .any(|hold| hold.source == ZONE && hold.kind == LibraryHoldKind::IdentityUnresolved)
    );
    let sparse = fixture.db.sparse_identities(ZONE).await.unwrap();
    assert_eq!(sparse.len(), 1);
    assert_eq!(sparse[0].source.as_str(), "asset-op-followup-hold");
    let source = followup
        .sources
        .iter()
        .find(|source| source.zone_name == ZONE)
        .unwrap();
    assert!(source.reached && source.operational_prework.is_some());
    let witness = source.sparse.as_ref().unwrap();
    assert!(witness.completed_delta_replay);
    fixture
        .db
        .attest_current_sparse(&witness.witness)
        .await
        .unwrap();
    let guards = fixture.finish_pair_tail(&mut followup).await;
    assert!(
        guards.is_some(),
        "all current source and journal owners must permit the follow-up hold"
    );
    let (health, report) =
        reported_health(&fixture, &followup, "catalog-positive-health.json").await;
    assert!(health.last_success_at.is_some());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(report["stats"]["failed"], 0);
    assert!(!followup.can_advance_database_checkpoint());
    let old_deadline: i64 = fixture
        .db
        .acquire_lock("recheck historical catalog obligation")
        .unwrap()
        .query_row(
            "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2",
            rusqlite::params![old.0, old.1],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_deadline, old.2);
    let receipt_still_retained: bool = fixture
        .db
        .acquire_lock("recheck peer completion receipt")
        .unwrap()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM download_receipts WHERE id=?1 AND length(proof_hash)=64)",
            [&receipt_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(receipt_still_retained);
    assert_eq!(preserved_pair_metadata(&fixture.db), metadata_before);
    assert_eq!(std::fs::read(&baseline_path).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&baseline_path)
            .unwrap()
            .modified()
            .unwrap(),
        baseline_mtime
    );
    assert_eq!(std::fs::read(&peer_path).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(&peer_path).unwrap().modified().unwrap(),
        peer_mtime
    );
    drop(guards);
}

#[tokio::test]
async fn mixed_health_refuses_actual_prepared_publication() {
    use crate::state::VersionSizeKey;
    use crate::state::db::provider_selection::SelectionPath;
    use crate::state::db::{PreparedPublication, TempFileOwnershipStore};
    use sha2::{Digest, Sha256};

    let (fixture, mut held) = mixed_refusal_fixture().await;
    let rows = admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap();
    let owner = rows.first().expect("actual admitted failed destination");
    let destination = fixture.root.join("progress.JPG");
    let temporary =
        crate::download::test_temp_download_path(&destination, &owner.checksum, ".pending-fixture")
            .unwrap();
    std::fs::write(&temporary, MEDIA).unwrap();
    fixture.db.claim_temp_file(&temporary).await.unwrap();
    let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
    let proof = PreparedPublication {
        binding: None,
        library: ZONE.to_owned(),
        id: owner.state_id.clone(),
        version: owner.version.clone(),
        provider_checksum: owner.checksum.clone(),
        provider_size: owner.size.try_into().unwrap(),
        temporary: SelectionPath::from_path(&temporary),
        destination: SelectionPath::from_path(&destination),
        local_checksum: checksum.clone(),
        source_checksum: checksum,
    };
    fixture
        .db
        .prepare_temp_publication(&temporary, &proof)
        .await
        .unwrap();
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_none());
    let (health, report) = reported_health(&fixture, &held, "publication-refusal.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(std::fs::read(&temporary).unwrap(), MEDIA);
    let retained = fixture
        .db
        .get_prepared_publications(
            ZONE,
            &owner.state_id,
            VersionSizeKey::from_str(&owner.version).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retained, vec![proof]);
}

#[tokio::test]
async fn mixed_health_refuses_owner_recorded_pending_layout() {
    use crate::state::db::primary_layout::{LayoutMember, LayoutOperation, path_key};
    use crate::state::db::provider_selection::SelectionPath;

    let (fixture, mut held) = mixed_refusal_fixture().await;
    let rows = admissions(&fixture.root.parent().unwrap().join("state.db")).unwrap();
    let owner = rows.first().expect("actual admitted failed destination");
    let destination = fixture.root.join("pending-layout.JPG");
    let record = crate::test_helpers::TestAssetRecord::new(&owner.state_id)
        .library(ZONE)
        .checksum(&owner.checksum)
        .size(owner.size.try_into().unwrap())
        .build();
    let operation = LayoutOperation {
        operation: "mixed-pending-layout".into(),
        family: "mixed-pending-family".into(),
        library: ZONE.into(),
        source_library: ZONE.into(),
        child: owner.state_id.clone(),
        asset_record_name: owner.child.clone(),
        pass: owner.pass.clone(),
        metadata_flags: 0,
        refresh_metadata: false,
        root: SelectionPath::from_path(&fixture.root),
        generation: 0,
        decision: "synthetic-recorded-decision".into(),
        policy: "primary".into(),
        qualifier: "-pending".into(),
        phase: "planned".into(),
        members: vec![LayoutMember {
            record: Some(record),
            destination: Some(SelectionPath::from_path(&destination)),
            path_key: Some(path_key(&destination).unwrap()),
            stage: Some(SelectionPath::from_path(
                &fixture.root.join("layout-stage.part"),
            )),
            old: None,
            archive_source: None,
            preserved_path: None,
            retirement: None,
            prepared: None,
            preserved: None,
            installed: None,
        }],
    };
    fixture.db.begin_primary_layout(operation).await.unwrap();
    assert_eq!(
        fixture
            .db
            .primary_layout_operations(ZONE.into())
            .await
            .unwrap()
            .len(),
        1
    );
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_none());
    let (health, report) = reported_health(&fixture, &held, "layout-refusal.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert_eq!(
        fixture
            .db
            .primary_layout_operations(ZONE.into())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn mixed_health_refuses_owner_recorded_metadata_failure() {
    let (fixture, mut held) = mixed_refusal_fixture().await;
    let peer = fixture.root.join("healthy-peer.JPG");
    let before_mtime = std::fs::metadata(&peer).unwrap().modified().unwrap();
    fixture
        .db
        .record_metadata_write_failure(ZONE, "asset-op-healthy-peer", "original")
        .await
        .unwrap();
    let pending = || {
        fixture
            .db
            .acquire_lock("inspect peer metadata failure")
            .unwrap()
            .query_row(
                "SELECT metadata_write_failed_at IS NOT NULL FROM assets \
                 WHERE library=?1 AND id='asset-op-healthy-peer' AND version_size='original'",
                [ZONE],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    };
    assert!(
        pending(),
        "the metadata owner must record actual pending work"
    );
    let guards = fixture.finish_mixed_tail(&mut held).await;
    assert!(guards.is_none());
    let (health, report) = reported_health(&fixture, &held, "metadata-refusal.json").await;
    assert!(health.last_success_at.is_none());
    assert_eq!(report["status"], "partial_failure");
    assert!(pending(), "the refusal must retain metadata work");
    assert_eq!(std::fs::read(&peer).unwrap(), MEDIA);
    assert_eq!(
        std::fs::metadata(peer).unwrap().modified().unwrap(),
        before_mtime
    );
}
