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
