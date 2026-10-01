//! Bounded, replayable event orderings across metadata-capture recovery.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rand::SeedableRng;
use rand::seq::SliceRandom;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::sync_cycle::run_cycle;
use crate::sync_loop::test_support::{
    MutableCaptureSession, RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions,
    full_album_page_with_download, make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle, media_without_photo_downloads,
};
use crate::{download, state};

#[derive(Clone, Copy, Debug)]
enum Event {
    Ambiguous,
    Deferred,
    MissingDate,
    NullDate,
    OutOfRangeDate,
    InterruptLookup,
    FailRefresh,
    Recover,
    Steady,
}

async fn run_capture_recovery_sequence(seed: u64) {
    const ZONE: &str = "SharedSync-SEQUENCE";
    const MASTER: &str = "master-sequence";
    const CHILD: &str = "asset-master-sequence";
    const TOKEN_KEY: &str = "sync_token:SharedSync-SEQUENCE";
    const CHECKSUM: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const RECOVERED_DATE: i64 = RUN_CYCLE_ASSET_DATE_MS + 42 * 86_400_000;

    // The prefix reaches a durable owner with an uncommitted metadata refresh.
    // Every shuffle then regresses provider evidence or interrupts recovery.
    // Recovery is deliberately separate from fault selection, so liveness has
    // a fixed bound and cannot be excused by an unlucky generated schedule.
    let mut faults = [
        Event::MissingDate,
        Event::NullDate,
        Event::OutOfRangeDate,
        Event::InterruptLookup,
        Event::FailRefresh,
    ];
    faults.shuffle(&mut rand::rngs::StdRng::seed_from_u64(seed));
    let mut trace = vec![Event::Ambiguous, Event::Deferred, Event::FailRefresh];
    trace.extend(faults);
    trace.extend([Event::Recover, Event::Steady, Event::Steady]);

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let media_path = media.join("legacy.jpg");
    let original_bytes = vec![0u8; 1024];
    std::fs::write(&media_path, &original_bytes).unwrap();
    let sidecar = media.join("unrelated.xmp");
    std::fs::write(&sidecar, b"private sidecar bytes").unwrap();
    let original_date = chrono::DateTime::from_timestamp(1_600_000_000, 0).unwrap();
    {
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        let row = crate::test_helpers::TestAssetRecord::new(MASTER)
            .library(ZONE)
            .filename("legacy.jpg")
            .created_at(original_date)
            .added_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
            .size(1024)
            .checksum(CHECKSUM)
            .build();
        db.upsert_seen(&row).await.unwrap();
        db.mark_downloaded(
            ZONE,
            MASTER,
            "original",
            &media_path,
            "local-checksum",
            None,
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping(ZONE, CHILD, MASTER)
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test(ZONE, MASTER, 0);
        db.set_metadata(TOKEN_KEY, "zone-tok-prev").await.unwrap();
    }

    let lookups = Arc::new(AtomicUsize::new(0));
    let config = make_run_cycle_config();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut completed_lookups = None;
    for (step, event) in trace.iter().copied().enumerate() {
        eprintln!("seed={seed} step={step} event={event:?} trace={trace:?}");
        let repaired = matches!(event, Event::Recover | Event::Steady);
        let cancel = CancellationToken::new();
        let mut page = full_album_page_with_download(
            ZONE,
            MASTER,
            "zone-tok-new",
            "https://p01.icloud-content.com/photo.jpg",
            1024,
            CHECKSUM,
        );
        let fields = &mut page["records"][1]["fields"];
        fields["isHidden"] = json!({"value": 1, "type": "INT64"});
        fields["isFavorite"] = json!({"value": 1, "type": "INT64"});
        // Valid fault attempts use a different date from the final recovery.
        fields["assetDate"] = json!({
            "value": if repaired { RECOVERED_DATE } else { RUN_CYCLE_ASSET_DATE_MS },
            "type": "TIMESTAMP"
        });
        match event {
            Event::Ambiguous | Event::MissingDate => {
                fields.as_object_mut().unwrap().remove("assetDate");
            }
            Event::NullDate => fields["assetDate"]["value"] = json!(null),
            Event::OutOfRangeDate => fields["assetDate"]["value"] = json!(1e100),
            _ => {}
        }
        if matches!(event, Event::Ambiguous) {
            let mut sibling = page["records"][1].clone();
            sibling["recordName"] = json!("valid-sibling");
            sibling["fields"]["isHidden"]["value"] = json!(0);
            sibling["fields"]["assetDate"] =
                json!({"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"});
            page["records"].as_array_mut().unwrap().push(sibling);
        }
        {
            let conn = rusqlite::Connection::open(&database).unwrap();
            conn.execute_batch("DROP TRIGGER IF EXISTS fail_sequence_refresh")
                .unwrap();
            if step == 2 {
                // Advance the existing deadline, without sleeping or changing
                // production retry policy. Provider-only changes do not
                // invalidate a still-current durable retry fingerprint.
                conn.execute("UPDATE metadata_capture_retries SET next_retry_at=0", [])
                    .unwrap();
            }
            if matches!(event, Event::FailRefresh) {
                conn.execute_batch(
                    "CREATE TRIGGER fail_sequence_refresh BEFORE UPDATE OF created_at ON assets BEGIN SELECT RAISE(ABORT, 'injected sequence refresh failure'); END;"
                ).unwrap();
            }
        }
        let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let db = inner.clone() as Arc<dyn download::DownloadStore>;
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            TOKEN_KEY,
            make_full_album_with_boxed_session(
                ZONE,
                Box::new(MutableCaptureSession {
                    zone: ZONE,
                    records: Arc::new(Mutex::new(page["records"].as_array().unwrap().clone())),
                    repair_requests: lookups.clone(),
                    cancel_after_lookup: matches!(event, Event::InterruptLookup)
                        .then(|| cancel.clone()),
                }),
            ),
        );
        let build = make_run_cycle_download_config_builder_with_options(
            &media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let result = Box::pin(run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &cancel,
        ))
        .await
        .unwrap();
        drop(build);
        drop(db);
        drop(inner);

        // Oracle: fixture facts, independent of the production selector and
        // counters. The final cycle is reopened too, not only intermediate ones.
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_ref(), MASTER);
        assert_eq!(
            rows[0].created_at,
            if repaired {
                chrono::DateTime::from_timestamp_millis(RECOVERED_DATE).unwrap()
            } else {
                original_date
            }
        );
        assert_eq!(
            rows[0].added_at.unwrap().timestamp_millis(),
            RUN_CYCLE_ASSET_DATE_MS
        );
        assert_eq!(rows[0].checksum.as_ref(), CHECKSUM);
        assert_eq!(rows[0].local_path.as_deref(), Some(media_path.as_path()));
        assert_eq!(rows[0].metadata.is_hidden, repaired);
        assert_eq!(rows[0].metadata.is_favorite, repaired);
        assert_eq!(
            db.get_legacy_master_state_owners().await.unwrap(),
            if step >= 2 {
                HashSet::from([(ZONE.to_string(), MASTER.to_string(), CHILD.to_string())])
            } else {
                HashSet::new()
            }
        );
        assert_eq!(
            db.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
            Some(if repaired {
                "zone-tok-new"
            } else {
                "zone-tok-prev"
            })
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(
            (
                summary.downloaded,
                summary.pending,
                summary.policy_excluded,
                summary.source_deleted
            ),
            (1, 0, 0, 0)
        );
        let capture = summary
            .metadata_capture
            .iter()
            .find(|item| item.library == ZONE)
            .unwrap();
        assert_eq!(
            capture.pending_revision,
            if repaired {
                None
            } else {
                Some(state::METADATA_CAPTURE_REVISION)
            }
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        let conn = rusqlite::Connection::open(&database).unwrap();
        let revision: i64 = conn.query_row(
            "SELECT revision FROM asset_metadata_capture_revisions WHERE library=?1 AND asset_id=?2",
            [ZONE, MASTER], |row| row.get(0),
        ).unwrap();
        assert_eq!(
            revision,
            if repaired {
                state::METADATA_CAPTURE_REVISION
            } else {
                0
            }
        );
        let retries: i64 = conn
            .query_row(
                "SELECT count(*) FROM metadata_capture_retries WHERE library=?1 AND asset_id=?2",
                [ZONE, MASTER],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retries, i64::from(!repaired));
        assert_eq!(std::fs::read(&media_path).unwrap(), original_bytes);
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"private sidecar bytes");
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 2);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(
            result.stats.metadata_capture_refreshed,
            usize::from(matches!(event, Event::Recover))
        );
        if matches!(event, Event::FailRefresh) {
            assert_eq!(result.stats.metadata_capture_failures, 1);
            assert!(
                capture
                    .last_error
                    .as_deref()
                    .unwrap()
                    .contains("injected sequence refresh failure")
            );
        }
        if matches!(event, Event::InterruptLookup) {
            assert!(cancel.is_cancelled());
            assert!(result.stats.interrupted);
        }
        if matches!(event, Event::Deferred) {
            // The provider now has one valid child, but the client has not
            // observed that change. Backoff must survive restart until due.
            assert_eq!(result.stats.metadata_capture_deferred, 1);
            assert_eq!(lookups.load(Ordering::SeqCst), 2);
        }
        if repaired {
            assert_eq!(result.failed_count, 0);
            assert!(!result.stats.identity_incomplete);
            assert_eq!(result.stats.metadata_capture_remaining, 0);
            let requests = lookups.load(Ordering::SeqCst);
            if let Some(previous) = completed_lookups {
                assert_eq!(requests, previous, "quiet stable tail");
            }
            completed_lookups = Some(requests);
        }
    }
}

#[tokio::test]
async fn capture_recovery_sequence_seed_862() {
    Box::pin(run_capture_recovery_sequence(862)).await;
}
#[tokio::test]
async fn capture_recovery_sequence_seed_873() {
    Box::pin(run_capture_recovery_sequence(873)).await;
}
#[tokio::test]
async fn capture_recovery_sequence_seed_869() {
    Box::pin(run_capture_recovery_sequence(869)).await;
}
#[tokio::test]
async fn capture_recovery_sequence_seed_870() {
    Box::pin(run_capture_recovery_sequence(870)).await;
}
