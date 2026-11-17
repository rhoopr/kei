#![cfg(test)]

//! Bounded, replayable event orderings across metadata-capture recovery.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use rand::seq::SliceRandom;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::download;
use crate::sync_cycle::run_cycle;
use crate::sync_loop::test_support::{
    MutableCaptureSession, RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions,
    full_album_page_with_download, make_owned_cycle_album, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle, open_owned_cycle_db,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Event {
    Ambiguous,
    Deferred,
    MissingDate,
    NullDate,
    OutOfRangeDate,
    MissingChild,
    VisibleInvalid,
    FilteredInvalid,
    InterruptLookup,
    FailRefresh,
    Recover,
    Steady,
}

async fn run_capture_history(seed: u64, trace: &[Event]) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Debug)]
    struct SequenceSession {
        session: MutableCaptureSession,
        changed: bool,
        token: String,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for SequenceSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/changes/zone?") {
                let request: serde_json::Value = serde_json::from_str(&body)?;
                let records = if !self.changed || request["zones"][0]["syncToken"] == self.token {
                    Vec::new()
                } else {
                    self.session.records.lock().unwrap().clone()
                };
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": self.session.zone, "ownerRecordName": "_defaultOwner"},
                    "syncToken": self.token, "moreComing": false,
                    "records": records
                }]}));
            }
            let mut response = self.session.post(url, body, headers).await?;
            if url.contains("/records/query?") {
                response["syncToken"] = json!(self.token);
            }
            Ok(response)
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    const ZONE: &str = "SharedSync-SEQUENCE";
    const MASTER: &str = "master-sequence";
    const CHILD: &str = "asset-master-sequence";
    const TOKEN_KEY: &str = "sync_token:SharedSync-SEQUENCE";
    const RECOVERED_DATE: i64 = RUN_CYCLE_ASSET_DATE_MS + 42 * 86_400_000;

    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("state.db");
    let media = dir.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let media_path = media.join("legacy.jpg");
    let original_bytes =
        b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xd9".to_vec();
    let checksum =
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&original_bytes));
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/photo.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(original_bytes.clone()))
        .mount(&server)
        .await;
    std::fs::write(&media_path, &original_bytes).unwrap();
    let sidecar = media.join("unrelated.xmp");
    std::fs::write(&sidecar, b"private sidecar bytes").unwrap();
    let original_date = chrono::DateTime::from_timestamp(1_600_000_000, 0).unwrap();
    {
        let db = open_owned_cycle_db(&database).await;
        let row = crate::test_helpers::TestAssetRecord::new(MASTER)
            .library(ZONE)
            .filename("legacy.jpg")
            .created_at(original_date)
            .added_at(chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap())
            .size(original_bytes.len() as u64)
            .checksum(&checksum)
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
        db.upsert_asset_master_mapping(ZONE, "historical-absent-child", MASTER)
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test(ZONE, MASTER, 0);
        db.set_metadata(TOKEN_KEY, "zone-tok-prev").await.unwrap();
        let historical = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(historical.len(), 1);
        assert_eq!(historical[0].id.as_ref(), MASTER);
        assert!(!historical[0].metadata.is_hidden);
        assert!(!historical[0].metadata.is_favorite);
    }

    let lookups = Arc::new(AtomicUsize::new(0));
    let config = make_run_cycle_config();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut completed_lookups = None;
    let mut completed_outputs = None;
    let mut checkpoint = "zone-tok-prev".to_owned();
    let mut current_date = RUN_CYCLE_ASSET_DATE_MS;
    // Admit the selected child through a real production download before
    // exercising malformed updates and failed current metadata transactions.
    for (step, event) in std::iter::once(Event::Deferred)
        .chain(trace.iter().copied())
        .enumerate()
    {
        eprintln!("seed={seed} step={step} event={event:?} trace={trace:?}");
        let repaired = matches!(event, Event::Recover | Event::Steady);
        let cancel = CancellationToken::new();
        // Failed steps retain the old cursor, so alternating two token values
        // can incorrectly describe a later mutation as an unchanged delta.
        let token = if repaired {
            "zone-tok-recovered".to_owned()
        } else {
            format!("sequence-{seed}-{step}")
        };
        let mut page = full_album_page_with_download(
            ZONE,
            MASTER,
            &token,
            &format!("{}/photo.jpg", server.uri()),
            original_bytes.len() as u64,
            &checksum,
        );
        let fields = &mut page["records"][1]["fields"];
        fields["masterRef"]["value"]["zoneID"]["ownerRecordName"] = json!("_defaultOwner");
        fields["isHidden"] = json!({"value": 1, "type": "INT64"});
        fields["isFavorite"] = json!({"value": 1, "type": "INT64"});
        if !matches!(event, Event::Deferred | Event::Recover | Event::Steady) {
            // A rejected date, identity, or transaction must reject the flags
            // from that same snapshot too, rather than partially refreshing it.
            fields["isFavorite"]["value"] = json!(0);
        }
        // Valid fault attempts use a different date from the final recovery.
        fields["assetDate"] = json!({
            "value": if repaired { RECOVERED_DATE } else if matches!(event, Event::FailRefresh | Event::InterruptLookup) { RECOVERED_DATE + 86_400_000 } else { RUN_CYCLE_ASSET_DATE_MS },
            "type": "TIMESTAMP"
        });
        match event {
            Event::Ambiguous
            | Event::MissingDate
            | Event::VisibleInvalid
            | Event::FilteredInvalid => {
                fields.as_object_mut().unwrap().remove("assetDate");
            }
            Event::NullDate => fields["assetDate"]["value"] = json!(null),
            Event::OutOfRangeDate => fields["assetDate"]["value"] = json!(1e100),
            _ => {}
        }
        if matches!(event, Event::VisibleInvalid) {
            fields["isHidden"]["value"] = json!(0);
        }
        if matches!(event, Event::MissingChild) {
            page["records"].as_array_mut().unwrap().truncate(1);
        }
        // InterruptLookup retains the usable master/child pair and an eligible
        // media resource. Its changed date and revision require confirmation;
        // cancellation occurs inside the provider lookup, before those facts
        // may replace the previously committed date and flags.
        if matches!(event, Event::Ambiguous) {
            let mut sibling = page["records"][1].clone();
            sibling["recordName"] = json!("valid-sibling");
            sibling["fields"]["isHidden"]["value"] = json!(0);
            sibling["fields"]["assetDate"] =
                json!({"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"});
            page["records"].as_array_mut().unwrap().push(sibling);
        }
        for record in page["records"].as_array_mut().unwrap() {
            record["recordChangeTag"] = json!(if repaired {
                "sequence-recovered".to_owned()
            } else {
                format!("sequence-{step}")
            });
        }
        {
            let conn = rusqlite::Connection::open(&database).unwrap();
            conn.execute_batch("DROP TRIGGER IF EXISTS fail_sequence_refresh")
                .unwrap();
            if matches!(event, Event::FailRefresh) {
                conn.execute_batch(
                    "CREATE TRIGGER fail_sequence_refresh BEFORE UPDATE OF created_at ON assets WHEN OLD.id='asset-master-sequence' BEGIN SELECT RAISE(ABORT, 'injected sequence refresh failure'); END;"
                ).unwrap();
            }
        }
        let inner = open_owned_cycle_db(&database).await;
        let db = inner.clone() as Arc<dyn download::DownloadStore>;
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            TOKEN_KEY,
            make_owned_cycle_album(
                &inner,
                ZONE,
                "shared",
                Box::new(SequenceSession {
                    changed: !matches!(event, Event::Steady),
                    token: token.clone(),
                    session: MutableCaptureSession {
                        // SequenceSession supplies the opaque per-event cursor
                        // for delta and query responses; lookup has no cursor.
                        token: "sequence-inner-unused",
                        zone: ZONE,
                        records: Arc::new(Mutex::new(page["records"].as_array().unwrap().clone())),
                        repair_requests: lookups.clone(),
                        cancel_after_lookup: matches!(event, Event::InterruptLookup)
                            .then(|| cancel.clone()),
                    },
                }),
            ),
        );
        let build = make_run_cycle_download_config_builder_with_options(
            &media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                skip_created_before: matches!(event, Event::FilteredInvalid).then_some(
                    crate::config::CreatedDateFilter::CaptureDate(
                        chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
                    ),
                ),
                ..RunCycleDownloadConfigOptions::default()
            },
        );
        let lookups_before = lookups.load(Ordering::SeqCst);
        let result = Box::pin(run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &cancel,
        ))
        .await
        .unwrap();
        drop(build);
        drop(library);
        drop(db);
        drop(inner);

        // Oracle: fixture facts, independent of the production selector and
        // counters. The final cycle is reopened too, not only intermediate ones.
        let db = open_owned_cycle_db(&database).await;
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        let legacy = rows
            .iter()
            .find(|row| row.id.as_ref() == MASTER)
            .expect("historical receipt retained");
        assert_eq!(
            legacy.created_at, original_date,
            "historical capture date is not selected work"
        );
        assert_eq!(
            legacy.added_at.unwrap().timestamp_millis(),
            RUN_CYCLE_ASSET_DATE_MS,
            "addition date preserved"
        );
        assert_eq!(
            legacy.checksum.as_ref(),
            checksum,
            "provider checksum preserved"
        );
        assert_eq!(
            legacy.local_path.as_deref(),
            Some(media_path.as_path()),
            "tracked path preserved"
        );
        assert!(!legacy.metadata.is_hidden);
        assert!(!legacy.metadata.is_favorite);
        assert!(
            db.get_legacy_master_state_owners()
                .await
                .unwrap()
                .is_empty(),
            "current child cannot adopt ambiguous history"
        );
        let selected_complete = matches!(event, Event::Deferred | Event::Recover | Event::Steady);
        if selected_complete {
            checkpoint.clone_from(&token);
            current_date = if repaired {
                RECOVERED_DATE
            } else {
                RUN_CYCLE_ASSET_DATE_MS
            };
        }
        assert_eq!(
            db.get_metadata(TOKEN_KEY).await.unwrap().as_deref(),
            Some(checkpoint.as_str()),
            "checkpoint requires completed selected work"
        );
        assert_eq!(
            result.stats.sync_token_blocked, !selected_complete,
            "selected source/update gate"
        );
        let current = rows
            .iter()
            .find(|row| row.id.as_ref() == CHILD)
            .expect("production-admitted child receipt");
        assert_eq!(
            current.created_at.timestamp_millis(),
            current_date,
            "malformed or failed updates cannot replace the durable date"
        );
        assert!(current.metadata.is_hidden);
        assert!(current.metadata.is_favorite);
        assert_ne!(current.local_path.as_deref(), Some(media_path.as_path()));
        assert_eq!(
            std::fs::read(current.local_path.as_ref().unwrap()).unwrap(),
            original_bytes
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
        assert_eq!(revision, 0, "historical capture receipt preserved");
        let retries: i64 = conn
            .query_row(
                "SELECT count(*) FROM metadata_capture_retries WHERE library=?1 AND asset_id=?2",
                [ZONE, MASTER],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retries, 0,
            "historical-only rows must not manufacture retries"
        );
        assert_eq!(
            std::fs::read(&media_path).unwrap(),
            original_bytes,
            "original media bytes"
        );
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            b"private sidecar bytes",
            "unrelated sidecar bytes"
        );
        let source: Option<String> = conn.query_row(
            "SELECT source_checksum FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND local_path=?3",
            rusqlite::params![ZONE, CHILD, current.local_path.as_ref().unwrap().to_str().unwrap()],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(
            source,
            Some(data_encoding::HEXLOWER.encode(&Sha256::digest(&original_bytes)))
        );
        if matches!(event, Event::FailRefresh) {
            assert!(
                result.stats.state_write_failures > 0 || result.failed_count > 0,
                "selected metadata write failure reported"
            );
        }
        if matches!(event, Event::InterruptLookup) {
            assert!(
                lookups.load(Ordering::SeqCst) > lookups_before,
                "interruption must reach the provider lookup boundary"
            );
            assert!(cancel.is_cancelled());
            assert!(result.stats.interrupted);
        }
        if repaired {
            assert_eq!(result.failed_count, 0, "recovery failure count");
            assert!(!result.stats.identity_incomplete);
            let requests = lookups.load(Ordering::SeqCst);
            if let Some(previous) = completed_lookups {
                assert_eq!(requests, previous, "quiet stable tail");
            }
            completed_lookups = Some(requests);
            let outputs: std::collections::BTreeMap<_, _> = rows
                .iter()
                .map(|row| {
                    let path = row.local_path.as_ref().unwrap();
                    (path.clone(), std::fs::read(path).unwrap())
                })
                .collect();
            assert_eq!(
                outputs.len(),
                rows.len(),
                "siblings retain independent publication paths"
            );
            if let Some(previous) = &completed_outputs {
                assert_eq!(
                    &outputs, previous,
                    "quiet tail preserves all published paths and bytes"
                );
            }
            completed_outputs = Some(outputs);
            if matches!(event, Event::Steady) {
                assert_eq!(result.stats.downloaded, 0, "quiet selected download tail");
                assert_eq!(
                    result.stats.metadata_capture_refreshed, 0,
                    "quiet capture tail"
                );
            }
        }
    }
}

async fn run_capture_recovery_sequence(seed: u64) {
    // The prefix reaches an independently downloaded child, then fails a selected refresh.
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

    Box::pin(run_capture_history(seed, &trace)).await;
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

// Public incident descriptions are reconstructed into synthetic fixture facts.
// None of these cases contains captured provider traffic, account DBs or photos.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconstructedHistory {
    name: String,
    provenance: String,
    public_issues: Vec<u64>,
    relationships: String,
    faults: Vec<Event>,
}

fn with_recovery_tail(faults: &[Event]) -> Vec<Event> {
    let mut trace = vec![Event::Ambiguous, Event::Deferred, Event::FailRefresh];
    trace.extend_from_slice(faults);
    trace.extend([Event::Recover, Event::Steady, Event::Steady]);
    trace
}

fn generated_faults(seed: u64) -> Vec<Event> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let vocabulary = [
        Event::MissingDate,
        Event::NullDate,
        Event::OutOfRangeDate,
        Event::InterruptLookup,
        Event::FailRefresh,
        Event::MissingChild,
        Event::VisibleInvalid,
        Event::FilteredInvalid,
        Event::Ambiguous,
    ];
    // Draw with replacement and vary length, rather than shuffling a fixed bag.
    // Repeated errors and oscillating evidence survive every SQLite reopen.
    (0..rng.random_range(1..=18))
        .map(|_| vocabulary[rng.random_range(0..vocabulary.len())])
        .collect()
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else {
        "non-string panic".to_string()
    }
}

async fn check_capture_history(seed: u64, faults: &[Event]) {
    assert!(
        faults.len() <= 18,
        "replay retains the generated length bound"
    );
    assert!(
        faults
            .iter()
            .all(|event| !matches!(event, Event::Deferred | Event::Recover | Event::Steady)),
        "replay faults cannot replace prerequisite or liveness events"
    );
    let trace = with_recovery_tail(faults);
    let result = std::panic::AssertUnwindSafe(run_capture_history(seed, &trace))
        .catch_unwind()
        .await;
    let Err(original) = result else { return };
    let signature = panic_message(original.as_ref());
    let mut reduced = faults.to_vec();
    let mut attempts = 0;
    let mut index = 0;
    // Bounded deletion reduction preserves the prerequisite state and mandatory
    // valid/quiet tail. Stable oracle labels distinguish assertion failures.
    while index < reduced.len() && attempts < 24 {
        let mut candidate = reduced.clone();
        candidate.remove(index);
        let candidate_trace = with_recovery_tail(&candidate);
        attempts += 1;
        let result = std::panic::AssertUnwindSafe(run_capture_history(seed, &candidate_trace))
            .catch_unwind()
            .await;
        if result
            .as_ref()
            .is_err_and(|payload| panic_message(payload.as_ref()) == signature)
        {
            reduced = candidate;
            index = 0;
        } else {
            index += 1;
        }
    }
    eprintln!(
        "Replay reduced faults with KEI_RECOVERY_FAULTS='{}' cargo test --lib capture_generated_histories -- --nocapture",
        serde_json::to_string(&reduced).unwrap()
    );
    eprintln!(
        "RECOVERY FAILURE seed={seed} original={trace:?} reduced={:?} deletion_attempts={attempts} assertion={signature}",
        with_recovery_tail(&reduced)
    );
    std::panic::resume_unwind(original);
}

#[tokio::test]
async fn capture_reconstructed_incident_histories() {
    let cases: Vec<ReconstructedHistory> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/recovery-histories.json"
    )))
    .unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        assert_eq!(case.provenance, "reconstructed-public-issue");
        assert!(!case.public_issues.is_empty());
        assert!(!case.relationships.is_empty());
        eprintln!(
            "reconstructed case={} issues={:?} relationships={}",
            case.name, case.public_issues, case.relationships
        );
        Box::pin(check_capture_history(862, &case.faults)).await;
    }
}

#[tokio::test]
async fn capture_generated_histories() {
    // Keep the default gate bounded. The full printed trace is the replay
    // contract, since RNG algorithms can change across dependency upgrades.
    if let Ok(faults) = std::env::var("KEI_RECOVERY_FAULTS") {
        let faults: Vec<Event> = serde_json::from_str(&faults).expect("JSON event array");
        Box::pin(check_capture_history(0, &faults)).await;
    } else {
        let seeds = std::env::var("KEI_RECOVERY_SEED").map_or_else(
            |_| vec![0, 1, 42, 765, 853, 861, 862, 20261001],
            |seed| vec![seed.parse::<u64>().expect("unsigned recovery seed")],
        );
        for seed in seeds {
            Box::pin(check_capture_history(seed, &generated_faults(seed))).await;
        }
    }
}
