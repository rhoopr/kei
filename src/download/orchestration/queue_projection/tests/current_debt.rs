use super::{
    CURRENT, DestinationSelectionSession, Fixture, controls, destination_pass,
    destination_pass_with_session, records, reopen_destination_fixture,
};
use crate::commands::AlbumPass;
use crate::download::{DownloadOutcome, SyncMode};
use crate::icloud::photos::PhotosSession;
use crate::sync_cycle::{CycleResult, run_cycle};
use crate::sync_loop::test_support::{
    make_captured_cycle_album, make_run_cycle_config, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use crate::test_helpers::DynamicRecentPhotosSession;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
const PRIMARY_KEY: &str = "sync_token:PrimarySync";
const SHARED_ZONE: &str = "SharedSync-recovery";
const SHARED_KEY: &str = "sync_token:SharedSync-recovery";

async fn cycle(f: &Fixture) -> crate::download::SyncResult {
    cycle_passes(f, std::slice::from_ref(&f.pass)).await
}

async fn cycle_passes(f: &Fixture, passes: &[AlbumPass]) -> crate::download::SyncResult {
    crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[derive(Clone)]
struct SplitFirstScan(DestinationSelectionSession, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl PhotosSession for SplitFirstScan {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let rank_query = url.contains("/records/query?");
        if rank_query
            && serde_json::from_str::<Value>(&body)?["query"]["filterBy"]
                .as_array()
                .is_some_and(|filters| {
                    filters
                        .iter()
                        .any(|f| f["fieldName"] == "startRank" && f["fieldValue"]["value"] == 0)
                })
        {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
        let mut response = self.0.post(url, body, headers).await?;
        if rank_query && self.1.load(Ordering::SeqCst) == 1 {
            response["syncToken"] = json!("split-first-scan");
        }
        Ok(response)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn repaired_first_baseline_token_is_held_by_an_older_deferred_selection() {
    check_repaired_baseline_after_deferral(Successor::DifferentPhoto).await;
}

#[tokio::test]
async fn repaired_first_baseline_accepts_an_exact_completed_successor() {
    check_repaired_baseline_after_deferral(Successor::Exact).await;
}

#[tokio::test]
async fn repaired_first_baseline_does_not_settle_a_renamed_photo() {
    check_repaired_baseline_after_deferral(Successor::Renamed).await;
}

#[tokio::test]
async fn repaired_first_baseline_keeps_hold_when_edited_url_becomes_alias() {
    check_repaired_baseline_after_deferral(Successor::AliasedEdit).await;
}

enum Successor {
    Exact,
    Renamed,
    DifferentPhoto,
    AliasedEdit,
}

fn add_edited_rendition(f: &Fixture, url: &str) {
    let mut records = f.session.records.lock().unwrap();
    let mut resource = records[0]["fields"]["resOriginalRes"].clone();
    resource["value"]["downloadURL"] = json!(url);
    records[1]["fields"]["resJPEGFullRes"] = resource;
    records[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
}

async fn check_repaired_baseline_after_deferral(successor: Successor) {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    let (trace, _guard) = crate::test_helpers::TracingCapture::install();
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = Arc::from("{album}");
    let config = make_run_cycle_config();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    f.db.acquire_lock("first baseline without a cursor")
        .unwrap()
        .execute("DELETE FROM metadata WHERE key=?1", [PRIMARY_KEY])
        .unwrap();
    f.db.set_metadata(SHARED_KEY, "shared-before")
        .await
        .unwrap();
    select(&f, "c", &server);
    if matches!(successor, Successor::AliasedEdit) {
        add_edited_rendition(&f, &format!("{}/media?edited", server.uri()));
        let body =
            serde_json::to_vec(&json!({"records": *f.session.records.lock().unwrap()})).unwrap();
        let (_, _, zone) = f.pass.album.owned_selection_scope().unwrap().unwrap();
        let observed = crate::icloud::photos::current_asset(&body, "asset-c", "c", &zone).unwrap();
        let config = f.config.with_pass(&destination_pass(&f, "A"));
        assert!(
            crate::download::filter::selected_resource_tasks(&observed, &config)
                .iter()
                .any(|task| task.version_size == crate::state::VersionSizeKey::Adjusted)
        );
    }
    *f.session.corrupt.lock().unwrap() = Some("missing-c");
    let ordinary_passes = |f: &Fixture| vec![destination_pass(f, "A"), destination_pass(f, "B")];
    let first = baseline_cycle(&f, &config, &session, ordinary_passes(&f)).await;
    assert!(first.stats.selected_work_remaining);
    assert_eq!(f.db.get_metadata(PRIMARY_KEY).await.unwrap(), None);
    assert_eq!(
        f.db.get_metadata(SHARED_KEY).await.unwrap().as_deref(),
        Some("shared-after"),
        "{first:?}"
    );
    let original: String = f
        .db
        .acquire_lock("deferred selection")
        .unwrap()
        .query_row(
            "SELECT generation FROM provider_active_decisions WHERE admission='deferred' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let evidence = historical_evidence(&f, &original);
    let has_deferred_hold = |retained| {
        trace.contains_event(|event| {
            event.field("diagnostic") == Some("selection_checkpoint_hold_v1")
                && event.field("blocker") == Some("deferred_identity")
                && event.field("retained_generation") == Some(retained)
        })
    };
    assert!(has_deferred_hold("false"));

    f = reopen_destination_fixture(f).await;
    let master = if matches!(successor, Successor::DifferentPhoto) {
        "m"
    } else {
        "c"
    };
    select(&f, master, &server);
    if matches!(successor, Successor::AliasedEdit) {
        add_edited_rendition(&f, &format!("{}/media", server.uri()));
    }
    if matches!(successor, Successor::Exact) {
        f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
            json!(format!("{}/media?renewed", server.uri()));
    }
    let filename = if matches!(successor, Successor::Renamed) {
        f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"]["value"] =
            json!("renamed.jpg");
        "renamed.JPG"
    } else {
        "changed.JPG"
    };
    *f.session.corrupt.lock().unwrap() = None;
    // Start a new generation without changing the selection configuration.
    f.capture(
        vec![json!({"recordName":"new-observation","recordType":"FutureOpaque"})],
        "change",
    )
    .await;
    let mut split = destination_pass_with_session(
        &f.session,
        "B",
        Box::new(SplitFirstScan(
            DestinationSelectionSession(f.session.clone()),
            Arc::default(),
        )),
    );
    split
        .album
        .set_shadow_capture(f.capture.clone(), Arc::from("private"));
    let result = baseline_cycle(
        &f,
        &config,
        &session,
        vec![destination_pass(&f, "A"), split],
    )
    .await;
    assert_eq!(result.stats.same_cycle_recovery_successes, 1, "{result:?}");
    assert_eq!(result.stats.downloaded, 2);
    assert_eq!(result.stats.failed, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(!result.stats.identity_incomplete);
    let summary = f.db.get_summary().await.unwrap();
    assert_eq!((summary.pending, summary.failed), (0, 0));
    assert_eq!(historical_evidence(&f, &original), evidence);
    if matches!(successor, Successor::AliasedEdit) {
        for album in ["A", "B"] {
            let edited = f.config.directory.join(album).join("changed_edited.JPG");
            assert!(!edited.exists());
        }
    }
    if matches!(successor, Successor::Exact) {
        assert_eq!(
            f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
            Some("destination-rank-proof"),
            "{result:?}"
        );
        assert!(!result.stats.selected_work_remaining);
        assert!(!has_deferred_hold("true"));
        let retained = retained_inventory(&f);
        f = reopen_destination_fixture(f).await;
        // Revisit the held decision when it would otherwise be due. The
        // settled history must not cause another lookup or rank inventory.
        f.db.make_selection_retry_due(super::owner(), "asset-c".into())
            .await
            .unwrap();
        for _ in 0..2 {
            let requests = (
                f.session.calls.load(Ordering::SeqCst),
                f.session.rank_calls.load(Ordering::SeqCst),
            );
            let quiet = baseline_cycle(&f, &config, &session, ordinary_passes(&f)).await;
            assert_eq!(quiet.failed_count, 0, "{quiet:?}");
            assert!(!quiet.stats.selected_work_remaining);
            assert_eq!(
                f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
                Some("quiet-zone-cursor")
            );
            assert_eq!(
                requests,
                (
                    f.session.calls.load(Ordering::SeqCst),
                    f.session.rank_calls.load(Ordering::SeqCst),
                )
            );
            assert_eq!(retained_inventory(&f), retained);
            assert_eq!(historical_evidence(&f, &original), evidence);
            f = reopen_destination_fixture(f).await;
        }
    } else {
        assert!(result.stats.selected_work_remaining);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some("selection_generation_deferred")
        );
        assert_eq!(f.db.get_metadata(PRIMARY_KEY).await.unwrap(), None);
        assert!(has_deferred_hold("true"));
    }
    for album in ["A", "B"] {
        assert_eq!(
            std::fs::read(f.config.directory.join(album).join(filename)).unwrap(),
            MEDIA
        );
    }
    assert_eq!(
        std::fs::read(f.config.directory.join("existing.jpg")).unwrap(),
        b"existing-media"
    );
    assert_eq!(
        std::fs::read(f.config.directory.join("existing.xmp")).unwrap(),
        b"retained-sidecar"
    );
    server.verify().await;
}

async fn baseline_cycle(
    f: &Fixture,
    config: &crate::config::Config,
    session: &crate::auth::SharedSession,
    primary_passes: Vec<AlbumPass>,
) -> CycleResult {
    let primary =
        make_run_cycle_library_state_with_passes("PrimarySync", PRIMARY_KEY, primary_passes);
    let shared = make_run_cycle_library_state_with_album(
        SHARED_ZONE,
        SHARED_KEY,
        make_captured_cycle_album(
            SHARED_ZONE,
            f.capture.clone(),
            Box::new(
                DynamicRecentPhotosSession::new(0)
                    .with_zone(SHARED_ZONE)
                    .with_token("shared-after"),
            ),
        ),
    );
    run_cycle(
        &[&primary, &shared],
        config,
        Some(f.db.as_ref()),
        &|mode, exclude_asset_ids, asset_groupings, library| {
            Arc::new(crate::download::DownloadConfig {
                sync_mode: mode,
                exclude_asset_ids,
                asset_groupings,
                library,
                ..f.config.clone()
            })
        },
        controls(),
        session,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

fn retained_inventory(f: &Fixture) -> (i64, i64, i64) {
    f.db.acquire_lock("measuring retained inventory")
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM provider_active_generations), \
                    (SELECT coalesce(sum(length(body)),0) FROM provider_selection_rank_pages), \
                    (SELECT coalesce(sum(length(manifest)),0) FROM provider_active_decisions)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

fn select(f: &Fixture, name: &str, server: &MockServer) {
    *f.session.records.lock().unwrap() = selected_records(name, server);
}

fn selected_records(name: &str, server: &MockServer) -> Vec<Value> {
    let mut selected = records(name, CURRENT, "selected debt proof");
    selected[0]["fields"]["resOriginalRes"]["value"] = json!({
        "downloadURL": format!("{}/media", server.uri()),
        "size": MEDIA.len(),
        "fileChecksum": data_encoding::BASE64.encode(&Sha256::digest(MEDIA)),
    });
    selected
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

#[derive(Clone)]
struct PagedSelection(super::CurrentSession);

#[async_trait::async_trait]
impl PhotosSession for PagedSelection {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch?") {
            let count = self.0.records.lock().unwrap().len() / 2;
            return Ok(json!({"batch":[{"records":[{"fields":{"itemCount":{"value":count}}}]}]}));
        }
        if url.contains("/records/query?") {
            self.0.rank_calls.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_str(&body)?;
            let offset = request["query"]["filterBy"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["fieldName"] == "startRank")
                .unwrap()["fieldValue"]["value"]
                .as_u64()
                .unwrap();
            let start = usize::try_from(offset)? * 2;
            let limit = usize::try_from(request["resultsLimit"].as_u64().unwrap())?;
            let records = self.0.records.lock().unwrap();
            let page = records.get(start..).unwrap_or_default();
            return Ok(json!({
                "records": &page[..page.len().min(limit)],
                "syncToken": "paged-selection-proof"
            }));
        }
        self.0.post(url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn use_paged_selection(f: &mut Fixture) {
    f.pass.kind = crate::commands::PassKind::Unfiled;
    f.pass.album = make_captured_cycle_album(
        "PrimarySync",
        f.capture.clone(),
        Box::new(PagedSelection(f.session.clone())),
    );
}

#[tokio::test]
async fn full_sync_410_keeps_healthy_transfers_running_before_cleanup() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expired"))
        .respond_with(ResponseTemplate::new(410))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.concurrent_downloads = 1;
    f.db.acquire_lock("first baseline without a checkpoint")
        .unwrap()
        .execute("DELETE FROM metadata WHERE key=?1", [PRIMARY_KEY])
        .unwrap();
    f.db.set_metadata(SHARED_KEY, "shared-before")
        .await
        .unwrap();
    use_paged_selection(&mut f);
    let mut expired = selected_records("expired", &server);
    expired[0]["fields"]["filenameEnc"]["value"] = json!("expired.jpg");
    expired[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/expired", server.uri()));
    let peers = ["one", "two"].into_iter().flat_map(|name| {
        let mut pair = selected_records(name, &server);
        pair[0]["fields"]["filenameEnc"]["value"] = json!(format!("{name}.jpg"));
        pair
    });
    *f.session.records.lock().unwrap() = expired.into_iter().chain(peers).collect();

    let config = make_run_cycle_config();
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let first = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
    let lookups = f.session.lookup_names.lock().unwrap().clone();
    let lookup_count = |record: &str| {
        lookups
            .iter()
            .filter(|name| name.as_str() == record)
            .count()
    };
    for healthy in ["asset-one", "asset-two"] {
        assert_eq!(
            lookup_count(healthy),
            1,
            "expected initial confirmation only, not cleanup: {lookups:?}"
        );
    }
    assert!(lookup_count("asset-expired") > 1, "{lookups:?}");
    assert_eq!((first.stats.downloaded, first.stats.failed), (2, 1));
    assert_eq!(first.failed_count, 1);
    assert!(!first.stats.interrupted);
    assert!(first.stats.selected_work_remaining);
    assert_eq!(f.db.get_metadata(PRIMARY_KEY).await.unwrap(), None);
    let summary = f.db.get_summary().await.unwrap();
    assert_eq!(
        (summary.downloaded, summary.pending + summary.failed),
        (2, 1)
    );
    assert_eq!(
        server.received_requests().await.unwrap()[0].url.path(),
        "/expired",
        "the 410 must precede healthy transfers"
    );
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/expired"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    for expected_downloads in [1, 0] {
        f = reopen_destination_fixture(f).await;
        use_paged_selection(&mut f);
        let recovered = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
        assert_eq!(
            recovered.stats.downloaded, expected_downloads,
            "{recovered:?}"
        );
        assert_eq!(recovered.failed_count, 0);
        assert!(f.db.get_metadata(PRIMARY_KEY).await.unwrap().is_some());
        let rows = f.db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert_eq!(std::fs::read(row.local_path.unwrap()).unwrap(), MEDIA);
        }
    }
    server.verify().await;
}

async fn settle_retained_fixture(
    f: &mut Fixture,
    selected: Vec<Value>,
    omitted: Vec<Value>,
) -> String {
    let original_count = (selected.len() + omitted.len()) / 2;
    *f.session.records.lock().unwrap() = selected.iter().cloned().chain(omitted).collect();
    *f.session.corrupt.lock().unwrap() = Some("missing");
    f.config.sync_mode = SyncMode::Full;
    assert!(cycle(f).await.checkpoint.sync_token_blocked);
    let (original, count): (String, i64) = f
        .db
        .acquire_lock("original deferred selection batch")
        .unwrap()
        .query_row(
            "SELECT generation,count(*) FROM provider_active_decisions WHERE admission='deferred' GROUP BY generation",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(usize::try_from(count).unwrap(), original_count);

    f.capture(
        vec![json!({"recordName":"next-inventory","recordType":"FutureOpaque"})],
        "next-inventory",
    )
    .await;
    let selected_count = selected.len() / 2;
    *f.session.records.lock().unwrap() = selected;
    *f.session.corrupt.lock().unwrap() = None;
    let current = cycle(f).await;
    assert_eq!(current.stats.downloaded, selected_count, "{current:?}");
    assert_eq!(current.stats.failed, 0, "{current:?}");
    assert_eq!(
        current.checkpoint.sync_token_blocked,
        selected_count != original_count
    );
    original
}

#[tokio::test]
async fn mixed_retained_lookup_batch_skips_settled_work_after_restart() {
    check_mixed_retained_lookup_batch(1, [true, false]).await;
}

#[tokio::test]
async fn retained_lookup_window_refills_within_candidate_budget() {
    // 64 settled entries leave room to claim the sibling in the first cycle.
    // 128 exhaust the separate probe budget; resume at the sibling on reopen.
    check_mixed_retained_lookup_batch(64, [true, false]).await;
    check_mixed_retained_lookup_batch(128, [false, true]).await;
}

async fn check_mixed_retained_lookup_batch(settled_count: u64, expected_lookups: [bool; 2]) {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(settled_count)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    use_paged_selection(&mut f);
    let names: Vec<_> = (0..settled_count).map(|n| format!("a{n:03}")).collect();
    let settled = names
        .iter()
        .flat_map(|name| {
            let mut pair = selected_records(name, &server);
            pair[0]["fields"]["filenameEnc"]["value"] = json!(format!("{name}.jpg"));
            pair
        })
        .collect();
    let unresolved = selected_records("z", &server);
    let original = settle_retained_fixture(&mut f, settled, unresolved).await;
    let evidence = historical_evidence(&f, &original);
    let retained = retained_inventory(&f);

    f = reopen_destination_fixture(f).await;
    for name in names.iter().map(String::as_str).chain(["z"]) {
        f.db.make_selection_retry_due(super::owner(), format!("asset-{name}"))
            .await
            .unwrap();
    }
    f.session.lookup_names.lock().unwrap().clear();
    let rank_calls = f.session.rank_calls.load(Ordering::SeqCst);
    for expected_lookup in expected_lookups {
        use_paged_selection(&mut f);
        let result = cycle(&f).await;
        assert!(result.stats.selected_work_remaining, "{result:?}");
        assert!(result.sync_token.is_none());
        assert_eq!(result.stats.downloaded, 0);
        let lookups = std::mem::take(&mut *f.session.lookup_names.lock().unwrap());
        if expected_lookup {
            assert!(lookups.iter().any(|name| name == "asset-z"), "{lookups:?}");
            assert!(
                lookups
                    .iter()
                    .all(|name| matches!(name.as_str(), "asset-z" | "z"))
            );
        } else {
            assert!(lookups.is_empty());
        }
        assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), rank_calls);
        assert_eq!(retained_inventory(&f), retained);
        assert_eq!(historical_evidence(&f, &original), evidence);
        f = reopen_destination_fixture(f).await;
    }
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
        Some("old-cursor")
    );
    server.verify().await;
}

#[tokio::test]
#[ignore = "synthetic retained checkpoint proof profile; run explicitly in release"]
async fn retained_checkpoint_successor_profile() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    for count in [256, 2048] {
        let mut f = Fixture::new().await;
        use_paged_selection(&mut f);
        f.config.concurrent_downloads = 8;
        let selected = (0..count)
            .flat_map(|n| {
                let name = format!("profile-{n:04}");
                let mut pair = selected_records(&name, &server);
                pair[0]["fields"]["filenameEnc"]["value"] = json!(format!("{name}.jpg"));
                pair
            })
            .collect();
        let old = settle_retained_fixture(&mut f, selected, Vec::new()).await;
        f = reopen_destination_fixture(f).await;
        for n in 0..count {
            f.db.make_selection_retry_due(super::owner(), format!("asset-profile-{n:04}"))
                .await
                .unwrap();
        }
        let (scope, hash, current): (String, String, String) =
            f.db.acquire_lock("selecting profile inputs")
                .unwrap()
                .query_row(
                    "SELECT scope,config_hash,id FROM provider_active_generations WHERE id<>?1",
                    [old],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
        let (_, rank_bytes, decision_bytes) = retained_inventory(&f);
        for _ in 0..3 {
            let start = Instant::now();
            assert!(
                f.db.retained_selection_root(
                    super::owner(),
                    scope.clone(),
                    hash.clone(),
                    current.clone()
                )
                .await
                .unwrap()
                .is_none()
            );
            let scheduler = start.elapsed();
            let start = Instant::now();
            assert!(
                f.db.selection_checkpoint_hold(super::owner(), scope.clone(), hash.clone(), false)
                    .await
                    .unwrap()
                    .is_none()
            );
            eprintln!(
                "{count} due lookups, {rank_bytes} rank bytes, {decision_bytes} decision bytes: scheduler {scheduler:?}, checkpoint {:?}",
                start.elapsed()
            );
        }
    }
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
    let (trace, _guard) = crate::test_helpers::TracingCapture::install();
    let held = cycle(&f).await;
    assert!(held.checkpoint.sync_token_blocked, "{held:?}");
    assert!(held.sync_token.is_none());
    assert!(trace.contains_event(|event| {
        event.field("diagnostic") == Some("selection_checkpoint_hold_v1")
            && event.field("blocker") == Some("prepared_publication")
            && event.field("retained_generation") == Some("true")
    }));
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
