//! Durable retry scheduling for ambiguous metadata-capture identities.

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use super::{
    MetadataCaptureCandidate, MetadataCaptureVersionEvidence, SqliteStateDb, StateError,
    VersionSizeKey,
};

const INITIAL_RETRY_SECONDS: i64 = 60 * 60;
const MAX_RETRY_SECONDS: i64 = 24 * INITIAL_RETRY_SECONDS;
const MAX_BACKOFF_SHIFT: u32 = 5;
const GENERATION_KEY: &str = "metadata_capture_retry_generation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MetadataCaptureRetryGeneration(i64);

struct Retry {
    evidence: String,
    target_revision: i64,
    attempts: u32,
    next_retry_at: i64,
}

#[derive(Default)]
pub(super) struct RetryCounts {
    pub(super) unresolved: u64,
    pub(super) deferred: u64,
}

// The sorted rendition tuples and resolved durable identity are compared, not
// interpreted as provider identity proof. No filenames or mutable metadata match.
fn evidence(candidate: &MetadataCaptureCandidate) -> String {
    let versions: Vec<_> = candidate
        .versions
        .iter()
        .map(|v| {
            (
                v.version_size.as_str(),
                v.checksum.as_str(),
                v.size_bytes,
                v.added_at.map(|date| date.timestamp_millis()),
            )
        })
        .collect();
    json!([
        candidate.master_record_name,
        candidate.asset_record_name,
        versions
    ])
    .to_string()
}

fn deferred(candidate: &MetadataCaptureCandidate, retry: &Retry, revision: i64, now: i64) -> bool {
    retry.target_revision == revision
        && retry.next_retry_at > now
        && retry.evidence == evidence(candidate)
}

// Keep the point predicate separate from the library scan so SQLite can use
// both columns of the (library, id, version_size) primary key.
fn candidate_query(asset_id: Option<&str>) -> String {
    let identity_filter = if asset_id.is_some() {
        " AND a.id=?3"
    } else {
        ""
    };
    format!(
        r"
        SELECT a.id, COALESCE(m.master_record_name,o.master_record_name,a.id),
               COALESCE(m.asset_record_name,o.asset_record_name),
               a.version_size,a.checksum,a.size_bytes,
               r.evidence,r.generation,r.attempts,r.next_retry_at,r.target_revision,a.added_at
        FROM assets a
        LEFT JOIN asset_metadata_capture_revisions v ON v.library=a.library AND v.asset_id=a.id
        LEFT JOIN asset_master_mappings m ON m.library=a.library AND m.asset_record_name=a.id
        LEFT JOIN legacy_master_state_owners o ON o.library=a.library AND o.master_record_name=a.id
        LEFT JOIN metadata_capture_retries r
          ON r.library=a.library AND r.asset_id=a.id
         AND r.target_revision=(
             SELECT MAX(rr.target_revision) FROM metadata_capture_retries rr
             WHERE rr.library=a.library AND rr.asset_id=a.id
               AND rr.target_revision<=?2
               AND (v.revision IS NULL OR v.revision<rr.target_revision)
         )
        WHERE a.library=?1 AND a.status='downloaded' AND a.is_deleted=0
          AND (v.revision IS NULL OR v.revision < ?2){identity_filter}
        ORDER BY a.id,a.version_size
    "
    )
}

const RETAINED_ASSET_IDS_SQL: &str = "SELECT DISTINCT asset_id FROM metadata_capture_retries \
    WHERE library=?1 AND target_revision<=?2 ORDER BY asset_id";

// Stream groups before applying the candidate budget. Deferred low-sorting IDs
// must not consume the limit and starve unattempted catalogue entries.
fn visit(
    conn: &Connection,
    library: &str,
    revision: i64,
    asset_id: Option<&str>,
    mut visitor: impl FnMut(MetadataCaptureCandidate, Option<Retry>) -> bool,
) -> Result<(), StateError> {
    let mut stmt = conn
        .prepare_cached(&candidate_query(asset_id))
        .map_err(|e| StateError::query("metadata_capture_retry::prepare", e))?;
    let mut rows = match asset_id {
        Some(id) => stmt.query(params![library, revision, id]),
        None => stmt.query(params![library, revision]),
    }
    .map_err(|e| StateError::query("metadata_capture_retry::query", e))?;
    let mut pending: Option<(MetadataCaptureCandidate, Option<Retry>)> = None;
    while let Some(row) = rows
        .next()
        .map_err(|e| StateError::query("metadata_capture_retry::row", e))?
    {
        let id: String = row.get(0)?;
        if pending
            .as_ref()
            .is_some_and(|(candidate, _)| candidate.asset_id != id)
            && let Some((candidate, retry)) = pending.take()
            && !visitor(candidate, retry)
        {
            return Ok(());
        }
        if pending.is_none() {
            let generation = row
                .get::<_, Option<i64>>(7)?
                .map(MetadataCaptureRetryGeneration);
            let retry = match generation {
                Some(_) => Some(Retry {
                    evidence: row.get(6)?,
                    target_revision: row.get(10)?,
                    attempts: row.get(8)?,
                    next_retry_at: row.get(9)?,
                }),
                None => None,
            };
            pending = Some((
                MetadataCaptureCandidate {
                    library: library.to_owned(),
                    asset_id: id,
                    master_record_name: row.get(1)?,
                    asset_record_name: row.get(2)?,
                    versions: Vec::new(),
                    retry_generation: generation,
                },
                retry,
            ));
        }
        let version: String = row.get(3)?;
        let version_size =
            VersionSizeKey::from_str(&version).ok_or_else(|| StateError::Invariant {
                operation: "metadata_capture_retry::version",
                detail: "unknown durable rendition key".into(),
            })?;
        if let Some((candidate, _)) = &mut pending {
            candidate.versions.push(MetadataCaptureVersionEvidence {
                added_at: row
                    .get::<_, Option<f64>>(11)?
                    .map(|date| super::decode_asset_date(date, 11))
                    .transpose()?,
                version_size,
                checksum: row.get(4)?,
                size_bytes: u64::try_from(row.get::<_, i64>(5)?).map_err(|_negative_size| {
                    StateError::Invariant {
                        operation: "metadata_capture_retry::size",
                        detail: "negative rendition size".into(),
                    }
                })?,
            });
        }
    }
    if let Some((candidate, retry)) = pending {
        visitor(candidate, retry);
    }
    Ok(())
}

pub(super) fn candidates(
    conn: &Connection,
    library: &str,
    revision: i64,
    limit: usize,
    now: i64,
) -> Result<Vec<MetadataCaptureCandidate>, StateError> {
    let mut due = Vec::new();
    if limit == 0 {
        return Ok(due);
    }
    visit(conn, library, revision, None, |candidate, retry| {
        if !retry
            .as_ref()
            .is_some_and(|retry| deferred(&candidate, retry, revision, now))
        {
            due.push(candidate);
        }
        due.len() < limit
    })?;
    Ok(due)
}

pub(super) fn counts(
    conn: &Connection,
    library: &str,
    revision: i64,
    now: i64,
) -> Result<RetryCounts, StateError> {
    let mut counts = RetryCounts::default();
    // Start from retained identities, not every stale catalogue row. An empty
    // queue must not hydrate the catalogue just to report zero retry counts.
    let mut stmt = conn.prepare_cached(RETAINED_ASSET_IDS_SQL)?;
    let ids = stmt.query_map(params![library, revision], |row| row.get::<_, String>(0))?;
    for id in ids {
        let id = id?;
        visit(conn, library, revision, Some(&id), |candidate, retry| {
            if let Some(retry) = retry {
                counts.unresolved += 1;
                counts.deferred += u64::from(deferred(&candidate, &retry, revision, now));
            }
            false
        })?;
    }
    Ok(counts)
}

pub(super) fn retire_completed(conn: &Connection, library: &str) -> Result<(), StateError> {
    conn.execute(r"DELETE FROM metadata_capture_retries AS r WHERE r.library=?1 AND NOT EXISTS (
        SELECT 1 FROM assets a LEFT JOIN asset_metadata_capture_revisions v ON v.library=a.library AND v.asset_id=a.id
        WHERE a.library=r.library AND a.id=r.asset_id AND a.status='downloaded' AND a.is_deleted=0
          AND (v.revision IS NULL OR v.revision < r.target_revision)
    )", [library]).map_err(|e| StateError::query("metadata_capture_retry::retire", e))?;
    Ok(())
}

impl SqliteStateDb {
    /// Retain ambiguity only while the selected identity, renditions, and retry
    /// generation still match. Returns false for stale or completed work.
    /// Database or generation errors leave the transaction unchanged.
    pub(crate) async fn defer_metadata_capture_ambiguity(
        &self,
        candidate: &MetadataCaptureCandidate,
        revision: i64,
    ) -> Result<bool, StateError> {
        let candidate = candidate.clone();
        self.with_conn_mut("defer_metadata_capture_ambiguity", move |conn| {
            let tx = conn.transaction()?;
            let mut current = None;
            visit(
                &tx,
                &candidate.library,
                revision,
                Some(&candidate.asset_id),
                |fresh, retry| {
                    current = Some((fresh, retry));
                    false
                },
            )?;
            let Some((fresh, retry)) = current else {
                return Ok(false);
            };
            // A concurrent repair, catalogue mutation, or another attempt must
            // not inherit this attempt's stale delay.
            if fresh != candidate {
                return Ok(false);
            }
            let evidence = evidence(&candidate);
            let previous_attempts = retry
                .as_ref()
                .filter(|r| r.target_revision == revision && r.evidence == evidence)
                .map_or(0, |r| r.attempts);
            let attempts = previous_attempts.saturating_add(1);
            let delay = (INITIAL_RETRY_SECONDS
                * (1_i64 << previous_attempts.min(MAX_BACKOFF_SHIFT)))
                .min(MAX_RETRY_SECONDS);
            let previous = tx
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [GENERATION_KEY],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let generation = previous
                .as_deref()
                .unwrap_or("0")
                .parse::<i64>()
                .ok()
                .filter(|v| *v >= 0)
                .and_then(|v| v.checked_add(1))
                .ok_or_else(|| StateError::Invariant {
                    operation: "defer_metadata_capture_ambiguity",
                    detail: "invalid or exhausted retry generation".into(),
                })?;
            tx.execute(
                "INSERT INTO metadata(key,value) VALUES (?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![GENERATION_KEY, generation.to_string()],
            )?;
            let now = Utc::now().timestamp();
            tx.execute(
                r"INSERT INTO metadata_capture_retries
                (library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                ON CONFLICT(library,asset_id,target_revision) DO UPDATE SET
                evidence=excluded.evidence,generation=excluded.generation,attempts=excluded.attempts,
                last_attempt_at=excluded.last_attempt_at,next_retry_at=excluded.next_retry_at",
                params![
                    candidate.library,
                    candidate.asset_id,
                    revision,
                    evidence,
                    generation,
                    attempts,
                    now,
                    now + delay,
                ],
            )?;
            tx.commit()?;
            Ok(true)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{SqliteStateDb, candidates, counts};
    use crate::state::METADATA_CAPTURE_REVISION;
    use crate::test_helpers::TestAssetRecord;

    async fn seed(db: &SqliteStateDb, id: &str) {
        let row = TestAssetRecord::new(id)
            .checksum("provider")
            .size(1024)
            .build();
        db.upsert_seen(&row).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            id,
            "original",
            Path::new("/photos/retained.jpg"),
            "local",
            None,
        )
        .await
        .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", id, 0);
        db.begin_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn metadata_capture_retry_bookkeeping_does_not_scan_unrelated_assets() {
        use rusqlite::StatementStatus;

        use super::{RETAINED_ASSET_IDS_SQL, candidate_query, visit};

        const MAX_POINT_QUERY_STEPS: i32 = 500;
        const MAX_EMPTY_QUEUE_STEPS: i32 = 100;
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed(&db, "Z").await;
        let selected = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        let point_query = candidate_query(Some("Z"));
        let library_query = candidate_query(None);
        {
            let conn = db.acquire_lock("retry_scale_fixture").unwrap();
            conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000)
                INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at)
                SELECT 'PrimarySync',printf('A%05d',x),'original','provider','retained.jpg',0,1024,'photo','downloaded',0 FROM n;").unwrap();
            conn.prepare_cached(&library_query)
                .unwrap()
                .reset_status(StatementStatus::VmStep);
            conn.prepare_cached(&point_query)
                .unwrap()
                .reset_status(StatementStatus::VmStep);
            conn.prepare_cached(RETAINED_ASSET_IDS_SQL)
                .unwrap()
                .reset_status(StatementStatus::VmStep);
            let empty = counts(&conn, "PrimarySync", 1, 0).unwrap();
            assert_eq!((empty.unresolved, empty.deferred), (0, 0));
            assert_eq!(
                conn.prepare_cached(&library_query)
                    .unwrap()
                    .reset_status(StatementStatus::VmStep),
                0
            );
            assert_eq!(
                conn.prepare_cached(&point_query)
                    .unwrap()
                    .reset_status(StatementStatus::VmStep),
                0
            );
            let queue_steps = conn
                .prepare_cached(RETAINED_ASSET_IDS_SQL)
                .unwrap()
                .reset_status(StatementStatus::VmStep);
            assert!(
                (1..MAX_EMPTY_QUEUE_STEPS).contains(&queue_steps),
                "empty queue steps: {queue_steps}"
            );

            let mut matched = Vec::new();
            visit(&conn, "PrimarySync", 1, Some("Z"), |candidate, _| {
                matched.push(candidate.asset_id);
                false
            })
            .unwrap();
            assert_eq!(matched, ["Z"]);
            let steps = conn
                .prepare_cached(&point_query)
                .unwrap()
                .reset_status(StatementStatus::VmStep);
            assert!(
                (1..MAX_POINT_QUERY_STEPS).contains(&steps),
                "point lookup steps: {steps}"
            );
        }
        // Exercise the production transaction's freshness read, then the
        // retained-only count reader on the same large, mostly unqueued library.
        assert!(
            db.defer_metadata_capture_ambiguity(&selected, 1)
                .await
                .unwrap()
        );
        let conn = db.acquire_lock("retry_scale_counts").unwrap();
        let steps = conn
            .prepare_cached(&point_query)
            .unwrap()
            .reset_status(StatementStatus::VmStep);
        assert!(
            (1..MAX_POINT_QUERY_STEPS).contains(&steps),
            "freshness lookup steps: {steps}"
        );
        let retained = counts(&conn, "PrimarySync", 1, 0).unwrap();
        assert_eq!((retained.unresolved, retained.deferred), (1, 1));
        let steps = conn
            .prepare_cached(&point_query)
            .unwrap()
            .reset_status(StatementStatus::VmStep);
        assert!(
            (1..MAX_POINT_QUERY_STEPS).contains(&steps),
            "retained lookup steps: {steps}"
        );
        assert_eq!(
            conn.prepare_cached(&library_query)
                .unwrap()
                .reset_status(StatementStatus::VmStep),
            0
        );
    }

    #[tokio::test]
    async fn metadata_capture_retry_is_durable_bounded_and_rejects_stale_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = SqliteStateDb::open(&path).await.unwrap();
        seed(&db, "A").await;
        seed(&db, "B").await;
        let selected = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(selected.asset_id, "A");
        assert!(
            db.defer_metadata_capture_ambiguity(&selected, 1)
                .await
                .unwrap()
        );
        assert!(
            !db.defer_metadata_capture_ambiguity(&selected, 1)
                .await
                .unwrap()
        );
        drop(db);
        let db = SqliteStateDb::open(&path).await.unwrap();
        assert_eq!(
            db.get_metadata_capture_candidates("PrimarySync", 1, 1)
                .await
                .unwrap()[0]
                .asset_id,
            "B"
        );
        let status = db
            .begin_metadata_capture_revision("PrimarySync", 1)
            .await
            .unwrap();
        assert_eq!(
            (
                status.remaining_assets,
                status.unresolved_assets,
                status.deferred_assets
            ),
            (2, 1, 1)
        );
        assert_eq!(status.pending_revision, Some(1));
        let encoded = serde_json::to_string(&status).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["unresolved_assets"], 1);
        assert_eq!(decoded["deferred_assets"], 1);
        for expected_delay in [3600, 7200, 14400, 28800, 57600, 86400, 86400] {
            let due = {
                let conn = db.acquire_lock("retry_test").unwrap();
                let (last, next): (i64, i64) = conn.query_row("SELECT last_attempt_at,next_retry_at FROM metadata_capture_retries WHERE asset_id='A'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
                assert_eq!(next - last, expected_delay);
                assert_eq!(
                    candidates(&conn, "PrimarySync", 1, 1, next - 1).unwrap()[0].asset_id,
                    "B"
                );
                candidates(&conn, "PrimarySync", 1, 1, next)
                    .unwrap()
                    .remove(0)
            };
            assert_eq!(due.asset_id, "A");
            assert!(db.defer_metadata_capture_ambiguity(&due, 1).await.unwrap());
        }
        let conn = db.acquire_lock("retry_test").unwrap();
        assert!(
            candidates(&conn, "OtherLibrary", 1, 10, i64::MAX)
                .unwrap()
                .is_empty()
        );
        assert_eq!(counts(&conn, "OtherLibrary", 1, 0).unwrap().unresolved, 0);
        assert_eq!(counts(&conn, "PrimarySync", 2, 0).unwrap().unresolved, 1);
        assert_eq!(counts(&conn, "PrimarySync", 2, 0).unwrap().deferred, 0);
        assert_eq!(candidates(&conn, "PrimarySync", 2, 10, 0).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn metadata_capture_retry_changed_evidence_is_due_and_old_attempt_cannot_delay_it() {
        for mutation in [
            "UPDATE assets SET checksum='changed' WHERE id='A'",
            "UPDATE assets SET size_bytes=2048 WHERE id='A'",
            "UPDATE assets SET added_at=1514764800.125 WHERE id='A'",
            "UPDATE metadata_capture_retries SET evidence=json_remove(evidence,'$[2][0][3]') WHERE asset_id='A'",
            "INSERT INTO legacy_master_state_owners VALUES ('PrimarySync','A','authoritative-child',0)",
        ] {
            let db = SqliteStateDb::open_in_memory().unwrap();
            seed(&db, "A").await;
            let selected = db
                .get_metadata_capture_candidates("PrimarySync", 1, 1)
                .await
                .unwrap()
                .remove(0);
            assert!(
                db.defer_metadata_capture_ambiguity(&selected, 1)
                    .await
                    .unwrap()
            );
            db.acquire_lock("retry_mutation")
                .unwrap()
                .execute_batch(mutation)
                .unwrap();
            assert!(
                !db.defer_metadata_capture_ambiguity(&selected, 1)
                    .await
                    .unwrap()
            );
            let due = db
                .get_metadata_capture_candidates("PrimarySync", 1, 1)
                .await
                .unwrap();
            assert_eq!(due.len(), 1);
            let status = db
                .begin_metadata_capture_revision("PrimarySync", 1)
                .await
                .unwrap();
            assert_eq!((status.unresolved_assets, status.deferred_assets), (1, 0));
        }
    }

    #[tokio::test]
    async fn metadata_capture_retry_generation_survives_retirement() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed(&db, "A").await;
        let first = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.defer_metadata_capture_ambiguity(&first, 1)
                .await
                .unwrap()
        );
        let old_attempt = candidates(
            &db.acquire_lock("retry_due").unwrap(),
            "PrimarySync",
            1,
            1,
            i64::MAX,
        )
        .unwrap()
        .remove(0);
        db.set_metadata_capture_revision_for_test("PrimarySync", "A", 1);
        db.complete_metadata_capture_revision("PrimarySync", 1)
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test("PrimarySync", "A", 0);
        let fresh = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(
            db.defer_metadata_capture_ambiguity(&fresh, 1)
                .await
                .unwrap()
        );
        let new_attempt = candidates(
            &db.acquire_lock("retry_due").unwrap(),
            "PrimarySync",
            1,
            1,
            i64::MAX,
        )
        .unwrap()
        .remove(0);
        assert_ne!(old_attempt.retry_generation, new_attempt.retry_generation);
        assert!(
            !db.defer_metadata_capture_ambiguity(&old_attempt, 1)
                .await
                .unwrap()
        );
        assert_eq!(
            db.begin_metadata_capture_revision("PrimarySync", 1)
                .await
                .unwrap()
                .unresolved_assets,
            1
        );
    }

    #[tokio::test]
    async fn metadata_capture_retry_write_failure_and_completion_preserve_work() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed(&db, "A").await;
        let selected = db
            .get_metadata_capture_candidates("PrimarySync", 1, 1)
            .await
            .unwrap()
            .remove(0);
        db.acquire_lock("retry_failure").unwrap().execute_batch("CREATE TRIGGER fail_retry BEFORE INSERT ON metadata_capture_retries BEGIN SELECT RAISE(ABORT,'test retry failure'); END;").unwrap();
        assert!(
            db.defer_metadata_capture_ambiguity(&selected, 1)
                .await
                .is_err()
        );
        assert_eq!(
            db.get_metadata_capture_candidates("PrimarySync", 1, 1)
                .await
                .unwrap(),
            vec![selected.clone()]
        );
        db.acquire_lock("retry_failure")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_retry;")
            .unwrap();
        assert!(
            db.defer_metadata_capture_ambiguity(&selected, 1)
                .await
                .unwrap()
        );
        db.set_metadata_capture_revision_for_test("PrimarySync", "A", 1);
        assert!(
            !db.defer_metadata_capture_ambiguity(&selected, 1)
                .await
                .unwrap()
        );
        let status = db
            .complete_metadata_capture_revision("PrimarySync", 1)
            .await
            .unwrap();
        assert_eq!(
            (
                status.remaining_assets,
                status.unresolved_assets,
                status.deferred_assets
            ),
            (0, 0, 0)
        );
        assert_eq!(status.pending_revision, None);
        assert_eq!(
            db.acquire_lock("retry_retired")
                .unwrap()
                .query_row("SELECT COUNT(*) FROM metadata_capture_retries", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
    }
}
