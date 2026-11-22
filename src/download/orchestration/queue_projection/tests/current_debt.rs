use super::{CURRENT, Fixture, controls, destination_pass, records, reopen_destination_fixture};
use crate::download::{DownloadOutcome, SyncMode};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";

async fn cycle(f: &Fixture) -> crate::download::SyncResult {
    crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn select(f: &Fixture, name: &str, server: &MockServer) {
    let mut selected = records(name, CURRENT, "selected debt proof");
    selected[0]["fields"]["resOriginalRes"]["value"] = json!({
        "downloadURL": format!("{}/media", server.uri()),
        "size": MEDIA.len(),
        "fileChecksum": data_encoding::BASE64.encode(&Sha256::digest(MEDIA)),
    });
    *f.session.records.lock().unwrap() = selected;
}

fn historical_evidence(f: &Fixture, generation: &str) -> Vec<Vec<u8>> {
    let conn =
        f.db.acquire_lock("retained original selection evidence")
            .unwrap();
    conn.prepare("SELECT specification FROM provider_active_generations WHERE id=?1 UNION ALL SELECT manifest FROM provider_active_decisions WHERE generation=?1 UNION ALL SELECT body FROM provider_selection_rank_pages WHERE generation=?1 ORDER BY 1")
        .unwrap()
        .query_map([generation], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn local_backfill_does_not_strand_an_updated_provider_rendition() {
    use crate::download::orchestration::generation::drain_local_backfill;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let newer = include_bytes!("../../../../../tests/data/media/pattern.jpg");
    Mock::given(method("GET"))
        .and(path("/update"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(newer.as_slice()))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.pass = destination_pass(&f, "A");
    f.config.post_sync_metadata = true;
    f.config.metadata.xmp_sidecar = true;
    select(&f, "m", &server);
    let first = cycle(&f).await;
    assert!(first.sync_token.is_some());
    f = reopen_destination_fixture(f).await;
    f.pass = destination_pass(&f, "A");
    let mut pair = records("m", CURRENT, "New caption");
    pair[0]["fields"]["filenameEnc"]["value"] = json!("new-version.jpg");
    pair[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/update",server.uri()),"size":newer.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(newer))});
    *f.session.records.lock().unwrap() = pair.clone();
    f.capture(pair, "new-version").await;
    f.config.sync_mode = SyncMode::Full;
    let updated = cycle(&f).await;
    if updated.stats.downloaded == 0 {
        let reasons: Vec<String> =
            f.db.acquire_lock("why replacement is waiting")
                .unwrap()
                .prepare("SELECT reason FROM provider_active_decisions WHERE admission='deferred'")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
        assert_eq!(reasons, ["unfinished_generation_conflict"]);
    }
    let pass = Box::pin(drain_local_backfill(
        std::slice::from_ref(&f.pass),
        &f.config,
        &f.db,
        controls(),
        &CancellationToken::new(),
        None,
        &mut Default::default(),
    ))
    .await
    .unwrap();
    assert!(!pass.storage_failed);
    if updated.stats.downloaded == 0 {
        // A still-owned historical file may fence replacing its catalogue
        // input, but its exact local work must remain reachable after reseed.
        f.db.make_selection_retry_due(super::owner(), "asset-m".into())
            .await
            .unwrap();
        let next = cycle(&f).await;
        assert_eq!(next.stats.downloaded, 1, "{updated:?} {next:?}");
    }
}

#[tokio::test]
async fn current_debt_configuration_change_retains_unknown_history_without_cursor_authority() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("{album}");
    f.pass = destination_pass(&f, "A");
    select(&f, "c", &server);
    *f.session.corrupt.lock().unwrap() = Some("missing-c");
    let unresolved = cycle(&f).await;
    assert!(unresolved.checkpoint.identity_incomplete);
    assert!(unresolved.checkpoint.sync_token_blocked);
    assert!(unresolved.sync_token.is_none());
    assert_eq!(unresolved.stats.downloaded, 0);
    let generation: String = f.db.acquire_lock("source-bound unknown generation").unwrap()
        .query_row("SELECT generation FROM provider_active_decisions WHERE child='asset-c' AND admission='deferred'", [], |row| row.get(0)).unwrap();
    let evidence = historical_evidence(&f, &generation);
    assert!(evidence.len() >= 3);

    // Unchanged current uncertainty still holds across a real connection reopen.
    f = reopen_destination_fixture(f).await;
    let still_current = cycle(&f).await;
    assert!(still_current.checkpoint.sync_token_blocked);
    assert!(still_current.sync_token.is_none());
    assert_eq!(historical_evidence(&f, &generation), evidence);

    // Make A due, so the absence of a lookup under B cannot be explained by backoff.
    f.db.make_selection_retry_due(super::owner(), "asset-c".into())
        .await
        .unwrap();
    let retry: (i64, i64) =
        f.db.acquire_lock("old retry preservation")
            .unwrap()
            .query_row(
                "SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1",
                [&generation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    select(&f, "m", &server);
    f.pass = destination_pass(&f, "B");
    let lookup_start = f.session.lookup_names.lock().unwrap().len();
    let completed = cycle(&f).await;
    assert!(
        matches!(completed.outcome, DownloadOutcome::Success),
        "{completed:?}"
    );
    assert!(!completed.checkpoint.sync_token_blocked, "{completed:?}");
    assert_eq!(completed.sync_token.as_deref(), Some("quiet-zone-cursor"));
    assert_eq!(completed.stats.downloaded, 1);
    {
        let names = f.session.lookup_names.lock().unwrap();
        let current = &names[lookup_start..];
        assert!(current.iter().any(|name| name == "asset-m"));
        assert!(
            current.iter().all(|name| name != "asset-c" && name != "c"),
            "{current:?}"
        );
    }
    let path = f.config.directory.join("B/changed.JPG");
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let calls = f.session.calls.load(Ordering::SeqCst);
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        f.pass = destination_pass(&f, "B");
        let quiet = cycle(&f).await;
        assert!(
            matches!(quiet.outcome, DownloadOutcome::Success),
            "{quiet:?}"
        );
        assert!(!quiet.checkpoint.sync_token_blocked, "{quiet:?}");
        assert_eq!(quiet.sync_token.as_deref(), Some("quiet-zone-cursor"));
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
        assert_eq!(historical_evidence(&f, &generation), evidence);
        let retained: (i64, i64) = f.db.acquire_lock("unchanged historical retry").unwrap()
            .query_row("SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND admission='deferred'", [&generation], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(retained, retry);
        assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        f.preserved().await;
    }
}

#[tokio::test]
async fn current_debt_configuration_change_keeps_interrupted_publication_fenced() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("A");
    f.pass = destination_pass(&f, "A");
    select(&f, "m", &server);
    f.db.acquire_lock("interrupt native completion after publication").unwrap().execute_batch(
        "CREATE TRIGGER current_debt_publication_fault BEFORE UPDATE OF verified_media ON provider_active_destinations WHEN NEW.verified_media=1 BEGIN SELECT RAISE(ABORT,'current debt publication fault'); END;"
    ).unwrap();
    let interrupted = cycle(&f).await;
    assert!(interrupted.checkpoint.sync_token_blocked, "{interrupted:?}");
    assert!(
        interrupted.stats.state_write_failures > 0,
        "{interrupted:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "initial publication phase"
    );
    let path = f.config.directory.join("A/changed.JPG");
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let generation: String =
        f.db.acquire_lock("unfinished native publication")
            .unwrap()
            .query_row(
                "SELECT generation FROM provider_active_destinations WHERE verified_media=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
    let evidence = historical_evidence(&f, &generation);
    f = reopen_destination_fixture(f).await;
    // Configuration drift cannot bypass the interrupted publication's writer
    // failure. B may attempt its admitted destination, but A's bytes stay intact.
    f.config.folder_structure_albums = Arc::from("{album}");
    let held = cycle(&f).await;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "reopened fault phase"
    );
    assert!(held.checkpoint.sync_token_blocked, "{held:?}");
    assert!(held.sync_token.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
    assert_eq!(historical_evidence(&f, &generation), evidence);
    f.db.acquire_lock("remove verified publication fault")
        .unwrap()
        .execute_batch("DROP TRIGGER current_debt_publication_fault")
        .unwrap();
    for _ in 0..3 {
        f = reopen_destination_fixture(f).await;
        f.config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "old-cursor".into(),
        };
        let recovered = cycle(&f).await;
        assert!(
            matches!(recovered.outcome, DownloadOutcome::Success),
            "{recovered:?}"
        );
        assert!(!recovered.checkpoint.sync_token_blocked, "{recovered:?}");
        assert_eq!(recovered.sync_token.as_deref(), Some("quiet-zone-cursor"));
        assert_eq!(recovered.stats.downloaded, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(historical_evidence(&f, &generation), evidence);
        f.preserved().await;
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn current_debt_prepared_output_fences_an_existing_selected_path_across_configuration_drift()
{
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("{album}");
    f.config.metadata.xmp_sidecar = true;
    f.pass = destination_pass(&f, "A");
    select(&f, "m", &server);
    let initial = cycle(&f).await;
    assert!(
        matches!(initial.outcome, DownloadOutcome::Success),
        "{initial:?}"
    );
    let (scope, config_hash): (String, String) =
        f.db.acquire_lock("completed sidecar selection")
            .unwrap()
            .query_row(
                "SELECT scope,config_hash FROM provider_active_generations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    f.config.metadata.embed_xmp = true;
    f.db.acquire_lock("interrupt prepared embedded publication").unwrap().execute_batch(
        "CREATE TRIGGER current_debt_prepared_fault BEFORE UPDATE OF verified_metadata ON provider_active_destinations WHEN NEW.verified_metadata=1 BEGIN SELECT RAISE(ABORT,'current debt prepared fault'); END;"
    ).unwrap();
    let interrupted = cycle(&f).await;
    assert!(interrupted.checkpoint.sync_token_blocked, "{interrupted:?}");
    let prepared: String = f.db.acquire_lock("actual unresolved prepared output").unwrap()
        .query_row("SELECT generation FROM provider_active_destinations WHERE prepared_hash IS NOT NULL AND verified_metadata=0", [], |row| row.get(0)).unwrap();
    let evidence = historical_evidence(&f, &prepared);
    let immutable_proof = |f: &Fixture| {
        f.db.acquire_lock("original decision and page identities").unwrap().prepare(
            "SELECT specification FROM provider_active_generations WHERE id=?1
             UNION ALL SELECT manifest FROM provider_active_decisions WHERE generation=?1
             UNION ALL SELECT CAST(id AS BLOB) FROM provider_selection_rank_pages WHERE generation=?1 ORDER BY 1"
        ).unwrap().query_map([&prepared], |row| row.get::<_, Vec<u8>>(0)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap()
    };
    let frozen = immutable_proof(&f);
    let path = f.config.directory.join("A/changed.JPG");
    let bytes = std::fs::read(&path).unwrap();
    assert_ne!(bytes, MEDIA);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    f = reopen_destination_fixture(f).await;
    f.config.metadata.embed_xmp = false;
    let calls = f.session.calls.load(Ordering::SeqCst);
    let held = cycle(&f).await;
    assert!(held.checkpoint.sync_token_blocked, "{held:?}");
    assert!(held.sync_token.is_none());
    assert_eq!(held.stats.downloaded, 0);
    assert_eq!(f.session.calls.load(Ordering::SeqCst), calls);
    assert!(
        f.db.unfinished_selection_debt(super::owner(), scope.clone(), config_hash.clone())
            .await
            .unwrap()
    );
    assert_eq!(historical_evidence(&f, &prepared), evidence);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    f.db.acquire_lock("remove prepared receipt fault")
        .unwrap()
        .execute_batch("DROP TRIGGER current_debt_prepared_fault")
        .unwrap();
    f = reopen_destination_fixture(f).await;
    f.config.metadata.embed_xmp = true;
    let recovered = cycle(&f).await;
    assert!(
        matches!(recovered.outcome, DownloadOutcome::Success),
        "{recovered:?}"
    );
    assert!(!recovered.checkpoint.sync_token_blocked, "{recovered:?}");
    assert_eq!(recovered.stats.downloaded, 0);
    // Re-enabling the frozen writer authorizes its pending metadata completion.
    // Subsequent disabled-embed cycles must preserve that completed output.
    let bytes = std::fs::read(&path).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let completed_checksum: String = f.db.acquire_lock("completed prepared writer receipt").unwrap()
        .query_row("SELECT local_checksum FROM provider_active_destinations WHERE generation=?1 AND verified_media=1 AND verified_metadata=1 AND prepared_hash IS NULL", [&prepared], |row| row.get(0)).unwrap();
    assert_eq!(
        completed_checksum,
        data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes))
    );
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        f.config.metadata.embed_xmp = false;
        let quiet = cycle(&f).await;
        assert!(
            matches!(quiet.outcome, DownloadOutcome::Success),
            "{quiet:?}"
        );
        assert_eq!(quiet.sync_token.as_deref(), Some("quiet-zone-cursor"));
        assert!(!quiet.checkpoint.sync_token_blocked);
        assert_eq!(quiet.stats.downloaded, 0);
        assert!(
            !f.db
                .unfinished_selection_debt(super::owner(), scope.clone(), config_hash.clone())
                .await
                .unwrap()
        );
        // Only the resolved writer's rank bodies may now be retired. The
        // exact decision, original page identities, and output remain frozen.
        assert_eq!(immutable_proof(&f), frozen);
        let retired: bool = f.db.acquire_lock("settled prepared generation")
            .unwrap().query_row("SELECT count(*)>0 AND count(*)=count(retired_proof_hash) FROM provider_selection_rank_pages WHERE generation=?1",
                [&prepared], |row| row.get(0)).unwrap();
        assert!(retired);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        f.preserved().await;
    }
}
