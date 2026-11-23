//! Ignored Linux release benchmark for sidecar-only local backfill reads.
//!
//! Run with:
//!
//! ```text
//! cargo test --release --lib \
//!   download::orchestration::queue_projection::tests::sidecar_bench::sidecar_only_backfill_linux_release_read_benchmark \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each media size downloads six ordinary selected destinations through local
//! `wiremock`, waits for the media mtimes to settle, then times only
//! `run_backfill_rows`. The three `missing` runs start without a sidecar and
//! must create one. The three `existing` runs start with the exact packet the
//! writer produced and must leave its bytes and mtime untouched. `/proc/self/io`
//! `rchar` deltas expose whole-media reads; fixture setup and the post-run byte
//! assertions stay outside the timed region.

use super::{CURRENT, Fixture, controls, destination_pass, owner, records};
use crate::download::metadata_rewrite::run_backfill_rows;
use crate::download::orchestration::generation::TEST_METADATA_WRITER_BUDGET;
use crate::download::pipeline::MetadataFlags;
use crate::state::db::PendingMetadataRewrite;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPS: usize = 3;
const MIB: usize = 1024 * 1024;

struct Case {
    label: &'static str,
    extension: &'static str,
    uti: &'static str,
    bytes: Vec<u8>,
}

/// Pattern MP4 plus a trailing, correctly sized ISO-BMFF `free` atom. The
/// container stays structurally valid while carrying realistic media size.
fn padded_mp4(target_len: usize) -> Vec<u8> {
    let base = include_bytes!("../../../../../tests/data/media/pattern.mp4");
    assert!(
        target_len >= base.len() + 8,
        "MP4 target {target_len} cannot hold the base file plus a free atom"
    );
    let free_len = target_len - base.len();
    let mut bytes = Vec::with_capacity(target_len);
    bytes.extend_from_slice(base);
    bytes.extend_from_slice(&u32::try_from(free_len).unwrap().to_be_bytes());
    bytes.extend_from_slice(b"free");
    bytes.resize(target_len, 0);
    assert_eq!(bytes.len(), target_len);
    bytes
}

fn small_jpeg() -> Vec<u8> {
    include_bytes!("../../../../../tests/data/media/pattern.jpg").to_vec()
}

fn sidecar_path(media: &Path) -> PathBuf {
    let mut name = media.as_os_str().to_owned();
    name.push(".xmp");
    PathBuf::from(name)
}

fn read_rchar() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .expect("Linux /proc/self/io must be readable")
        .lines()
        .find_map(|line| line.strip_prefix("rchar:"))
        .expect("rchar must describe process reads")
        .trim()
        .parse()
        .expect("rchar must be an unsigned byte count")
}

async fn timed_run(
    f: &Fixture,
    flags: MetadataFlags,
    cancel: &CancellationToken,
    generation: &str,
    row: PendingMetadataRewrite,
) -> (
    crate::download::metadata_rewrite::RewritePass,
    Duration,
    u64,
) {
    let rchar_before = read_rchar();
    let started = Instant::now();
    let pass = Box::pin(tokio::time::timeout(
        Duration::from_secs(60),
        run_backfill_rows(
            f.db.as_ref(),
            flags,
            f.config.temp_suffix.clone(),
            cancel,
            vec![row],
            Some(generation),
        ),
    ))
    .await
    .expect("one queued sidecar writer must finish within 60s");
    let elapsed = started.elapsed();
    let read = read_rchar().saturating_sub(rchar_before);
    (pass, elapsed, read)
}

fn assert_one_writer_and_verified(f: &Fixture, generation: &str, media: &Path) {
    let path_key = media.to_string_lossy();
    let (verified, writers): (bool, i64) = {
        let conn = f.db.acquire_lock("sidecar bench durable metadata").unwrap();
        let verified = conn
            .query_row(
                "SELECT verified_metadata FROM provider_active_destinations \
                 WHERE generation=?1 AND compat_path=?2 AND admitted=1",
                [generation, path_key.as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        let writers = conn
            .query_row(
                "SELECT count(*) FROM provider_active_destinations \
                 WHERE generation=?1 AND compat_path=?2 AND admitted=1",
                [generation, path_key.as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        (verified, writers)
    };
    assert!(
        verified,
        "sidecar writer must durably verify metadata for {}",
        media.display()
    );
    assert_eq!(
        writers,
        1,
        "exactly one physical writer owns {}",
        media.display()
    );
}

fn assert_media_preserved(media: &Path, bytes: &[u8], modified: std::time::SystemTime) {
    assert_eq!(
        std::fs::read(media).unwrap(),
        bytes,
        "sidecar-only work must not rewrite media bytes"
    );
    assert_eq!(
        std::fs::metadata(media).unwrap().modified().unwrap(),
        modified,
        "sidecar-only work must not change media mtime"
    );
}

fn take_row(
    rows: &mut BTreeMap<String, PendingMetadataRewrite>,
    stem: &str,
) -> PendingMetadataRewrite {
    let key = rows
        .keys()
        .find(|name| name.split_once('.').is_some_and(|(base, _)| base == stem))
        .cloned()
        .unwrap_or_else(|| panic!("no pending row for {stem}; have {:?}", rows.keys()));
    rows.remove(&key).unwrap()
}

async fn run_case(case: Case) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(case.bytes.clone()))
        .mount(&server)
        .await;

    let mut f = Fixture::new().await;
    f.config.post_sync_metadata = true;
    f.config.metadata.xmp_sidecar = true;
    f.config.metadata.embed_xmp = false;

    let checksum = data_encoding::BASE64.encode(&Sha256::digest(&case.bytes));
    let mut session_records = Vec::new();
    for mode in ["missing", "existing"] {
        for rep in 0..REPS {
            let name = format!("{}-{mode}-{rep}", case.label);
            let filename = format!("{name}.{}", case.extension);
            let mut pair = records(&name, CURRENT, "Sidecar benchmark caption");
            pair[0]["fields"]["filenameEnc"]["value"] = json!(filename);
            pair[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL": server.uri(),
                "size": case.bytes.len(),
                "fileChecksum": checksum,
            });
            pair[0]["fields"]["itemType"]["value"] = json!(case.uti);
            pair[0]["fields"]["resOriginalFileType"]["value"] = json!(case.uti);
            session_records.push(pair);
        }
    }
    *f.session.records.lock().unwrap() = session_records.into_iter().flatten().collect();
    f.pass = destination_pass(&f, "A");

    let result = tokio::time::timeout(
        Duration::from_secs(180),
        TEST_METADATA_WRITER_BUDGET.scope(
            0,
            Box::pin(crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                std::slice::from_ref(&f.pass),
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )),
        ),
    )
    .await
    .expect("fixture sync must finish within 180s")
    .unwrap();
    assert_eq!(
        result.stats.downloaded,
        2 * REPS,
        "all fixture media must transfer through local wiremock: {result:?}"
    );

    let generation: String =
        f.db.acquire_lock("sidecar bench generation")
            .unwrap()
            .query_row(
                "SELECT id FROM provider_active_generations \
             ORDER BY created_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
    let pending_media: i64 =
        f.db.acquire_lock("sidecar bench pending media")
            .unwrap()
            .query_row(
                "SELECT count(*) FROM provider_active_destinations \
             WHERE generation=?1 AND admitted=1 AND verified_media=1 AND verified_metadata=0",
                [&generation],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(pending_media, i64::try_from(2 * REPS).unwrap());
    let preserved_local: i64 =
        f.db.acquire_lock("sidecar bench preserved local")
            .unwrap()
            .query_row(
                "SELECT count(*) FROM provider_active_destinations \
             WHERE generation=?1 AND admitted=1 AND preserved_local IS NOT NULL",
                [&generation],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(
        preserved_local, 0,
        "fresh selected media must not carry preserved-local evidence"
    );

    let pending =
        f.db.pending_selection_metadata(owner(), generation.clone(), 250)
            .await
            .unwrap();
    assert_eq!(pending.len(), 2 * REPS);
    let mut rows_by_name: BTreeMap<String, PendingMetadataRewrite> = BTreeMap::new();
    for row in pending {
        assert!(
            row.selection_receipt.is_some(),
            "ordinary selected queue rows must carry a receipt"
        );
        let path = row
            .asset
            .local_path
            .as_deref()
            .expect("verified media must have a local path");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("fixture path has a UTF-8 name")
            .to_owned();
        assert!(
            rows_by_name.insert(name, row).is_none(),
            "each fixture destination is unique"
        );
    }

    for row in rows_by_name.values() {
        let media = row.asset.local_path.as_deref().unwrap();
        match std::fs::remove_file(sidecar_path(media)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("could not reset fixture sidecar: {error}"),
        }
    }

    // Stable old media: give mtime-sensitive eligibility checks >1s of age.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    let flags = MetadataFlags::from(&f.config);
    let cancel = CancellationToken::new();
    let mut seed_packet = None;

    for rep in 0..REPS {
        let stem = format!("{}-missing-{rep}", case.label);
        let row = take_row(&mut rows_by_name, &stem);
        let media = row.asset.local_path.clone().unwrap();
        let sidecar = sidecar_path(&media);
        assert!(
            !sidecar.exists(),
            "missing slot must start without a sidecar: {}",
            sidecar.display()
        );
        let media_bytes = std::fs::read(&media).unwrap();
        let media_modified = std::fs::metadata(&media).unwrap().modified().unwrap();

        let (pass, elapsed, read) = timed_run(&f, flags, &cancel, &generation, row).await;
        eprintln!(
            "SIDECAR_BENCH label={} mode=missing rep={} snapshot_bytes={} elapsed_us={} rchar_delta={}",
            case.label,
            rep,
            case.bytes.len(),
            elapsed.as_micros(),
            read
        );

        assert_eq!(pass.fetched, 1);
        assert_eq!(pass.applied, 1);
        assert_eq!(pass.failed, 0);
        assert_eq!(pass.retired_from_selected_queue, 1);
        assert!(!pass.state_failed);
        assert!(!pass.storage_failed);
        assert_media_preserved(&media, &media_bytes, media_modified);
        let created = std::fs::read(&sidecar).expect("missing mode must create the sidecar");
        assert!(!created.is_empty(), "created sidecar must not be empty");
        if rep == 0 {
            seed_packet = Some(created);
        }
        assert_one_writer_and_verified(&f, &generation, &media);
    }

    let packet = seed_packet.expect("the first missing run seeds the existing mode");
    for rep in 0..REPS {
        let stem = format!("{}-existing-{rep}", case.label);
        let row = take_row(&mut rows_by_name, &stem);
        let media = row.asset.local_path.clone().unwrap();
        let sidecar = sidecar_path(&media);
        std::fs::write(&sidecar, &packet).unwrap();
        let sidecar_modified = std::fs::metadata(&sidecar).unwrap().modified().unwrap();
        let media_bytes = std::fs::read(&media).unwrap();
        let media_modified = std::fs::metadata(&media).unwrap().modified().unwrap();

        let (pass, elapsed, read) = timed_run(&f, flags, &cancel, &generation, row).await;
        eprintln!(
            "SIDECAR_BENCH label={} mode=existing rep={} snapshot_bytes={} elapsed_us={} rchar_delta={}",
            case.label,
            rep,
            case.bytes.len(),
            elapsed.as_micros(),
            read
        );

        assert_eq!(pass.fetched, 1);
        assert_eq!(pass.applied, 1);
        assert_eq!(pass.failed, 0);
        assert_eq!(pass.retired_from_selected_queue, 1);
        assert!(!pass.state_failed);
        assert!(!pass.storage_failed);
        assert_media_preserved(&media, &media_bytes, media_modified);
        assert_eq!(
            std::fs::read(&sidecar).unwrap(),
            packet,
            "unchanged mode must preserve exact sidecar bytes"
        );
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().modified().unwrap(),
            sidecar_modified,
            "unchanged mode must not rewrite the sidecar"
        );
        assert_one_writer_and_verified(&f, &generation, &media);
    }

    assert!(rows_by_name.is_empty());
    let outstanding: i64 =
        f.db.acquire_lock("sidecar bench durable completion")
            .unwrap()
            .query_row(
                "SELECT count(*) FROM provider_active_destinations \
             WHERE generation=?1 AND admitted=1 AND (verified_media=0 OR verified_metadata=0)",
                [&generation],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(outstanding, 0, "every destination must verify durably");
    assert!(
        f.db.pending_selection_metadata(owner(), generation, 250)
            .await
            .unwrap()
            .is_empty(),
        "no queued ordinary metadata work may remain"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "ignored Linux release sidecar read benchmark; run explicitly"]
async fn sidecar_only_backfill_linux_release_read_benchmark() {
    Box::pin(run_case(Case {
        label: "small-jpeg",
        extension: "jpg",
        uti: "public.jpeg",
        bytes: small_jpeg(),
    }))
    .await;
    Box::pin(run_case(Case {
        label: "mp4-8mib",
        extension: "mp4",
        uti: "public.mpeg-4",
        bytes: padded_mp4(8 * MIB),
    }))
    .await;
    Box::pin(run_case(Case {
        label: "mp4-64mib",
        extension: "mp4",
        uti: "public.mpeg-4",
        bytes: padded_mp4(64 * MIB),
    }))
    .await;
}
