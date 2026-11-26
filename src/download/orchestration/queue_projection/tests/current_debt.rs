use super::{
    CURRENT, DestinationSelectionSession, Fixture, controls, destination_pass,
    destination_pass_with_session, records, reopen_destination_fixture,
};
use crate::commands::AlbumPass;
use crate::download::{DownloadOutcome, SyncMode};
use crate::icloud::photos::PhotosSession;
use crate::state::ReportStateStore;
use crate::state::SyncTokenStore;
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
async fn repaired_first_baseline_retires_an_omitted_plan() {
    check_repaired_baseline_after_deferral(Successor::DifferentPhoto {
        fail_checkpoint: false,
    })
    .await;
}

#[tokio::test]
async fn baseline_inventory_authority_and_zone_checkpoint_commit_together() {
    check_repaired_baseline_after_deferral(Successor::DifferentPhoto {
        fail_checkpoint: true,
    })
    .await;
}

#[tokio::test]
async fn repaired_first_baseline_accepts_an_exact_completed_successor() {
    check_repaired_baseline_after_deferral(Successor::Exact).await;
}

#[tokio::test]
async fn repaired_first_baseline_accepts_a_rename_before_path_admission() {
    check_repaired_baseline_after_deferral(Successor::Renamed).await;
}

#[tokio::test]
async fn repaired_first_baseline_uses_current_edited_url_alias() {
    check_repaired_baseline_after_deferral(Successor::AliasedEdit).await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn repaired_first_baseline_needs_no_writer_for_the_omitted_photo() {
    check_repaired_baseline_after_deferral(Successor::DifferentPhotoWithWriter).await;
}

enum Successor {
    Exact,
    Renamed,
    DifferentPhoto {
        fail_checkpoint: bool,
    },
    #[cfg(feature = "xmp")]
    DifferentPhotoWithWriter,
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
    #[cfg(feature = "xmp")]
    if matches!(successor, Successor::DifferentPhotoWithWriter) {
        f.config.metadata.xmp_sidecar = true;
        f.config.post_sync_metadata = true;
    }
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
    if matches!(successor, Successor::DifferentPhoto { .. }) {
        f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"]["value"] =
            json!("retained.jpg");
    }
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
    let first = baseline_cycle(&f, &config, &session, ordinary_passes(&f)).await;
    assert!(first.stats.selected_work_remaining);
    assert_eq!(f.db.get_metadata(PRIMARY_KEY).await.unwrap(), None);
    assert_eq!(
        f.db.get_metadata(SHARED_KEY).await.unwrap().as_deref(),
        Some("shared-after"),
        "{first:?}"
    );
    let (original, hash): (String, String) = f
        .db
        .acquire_lock("deferred selection")
        .unwrap()
        .query_row(
            "SELECT g.id,g.config_hash FROM provider_active_decisions d
             JOIN provider_active_generations g ON g.id=d.generation WHERE d.admission='deferred' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let evidence = historical_evidence(&f, &original);
    let authority_key = format!("selection_inventory_v1:{hash}");
    assert_eq!(f.db.get_metadata(&authority_key).await.unwrap(), None);
    let has_deferred_hold = |retained| {
        trace.contains_event(|event| {
            event.field("diagnostic") == Some("selection_checkpoint_hold_v1")
                && event.field("blocker") == Some("deferred_identity")
                && event.field("retained_generation") == Some(retained)
        })
    };
    assert!(has_deferred_hold("false"));

    f = reopen_destination_fixture(f).await;
    f.db.make_selection_retry_due(super::owner(), "asset-c".into())
        .await
        .unwrap();
    f.session.lookup_names.lock().unwrap().clear();
    let master = match successor {
        Successor::DifferentPhoto { .. } => "m",
        #[cfg(feature = "xmp")]
        Successor::DifferentPhotoWithWriter => "m",
        _ => "c",
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
    let change = if matches!(successor, Successor::DifferentPhoto { .. }) {
        selected_records("m", &server)
    } else {
        vec![json!({"recordName":"new-observation","recordType":"FutureOpaque"})]
    };
    f.capture(change, "change").await;
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
    let fail_checkpoint = matches!(
        successor,
        Successor::DifferentPhoto {
            fail_checkpoint: true
        }
    );
    if fail_checkpoint {
        f.db.acquire_lock("inject inventory authority write failure")
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER fail_inventory_authority BEFORE INSERT ON metadata
                 WHEN NEW.key GLOB 'selection_inventory_v1:*'
                 BEGIN SELECT RAISE(ABORT,'inventory checkpoint interrupted'); END;",
            )
            .unwrap();
    }
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
    if master == "m" {
        assert!(
            f.session
                .lookup_names
                .lock()
                .unwrap()
                .iter()
                .all(|name| !matches!(name.as_str(), "asset-c" | "c"))
        );
    }
    let summary = f.db.get_summary().await.unwrap();
    assert_eq!((summary.pending, summary.failed), (0, 0));
    assert_eq!(historical_evidence(&f, &original), evidence);
    if matches!(successor, Successor::AliasedEdit) {
        for album in ["A", "B"] {
            let edited = f.config.directory.join(album).join("changed_edited.JPG");
            assert!(!edited.exists());
        }
    }
    if fail_checkpoint {
        assert!(result.stats.state_write_failures > 0, "{result:?}");
        assert_eq!(f.db.get_metadata(&authority_key).await.unwrap(), None);
        assert_eq!(f.db.get_metadata(PRIMARY_KEY).await.unwrap(), None);
        f.db.acquire_lock("release inventory authority write failure")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_inventory_authority")
            .unwrap();
        f = reopen_destination_fixture(f).await;
        let retry = baseline_cycle(&f, &config, &session, ordinary_passes(&f)).await;
        assert_eq!(retry.failed_count, 0, "{retry:?}");
        assert_eq!(retry.stats.downloaded, 0, "{retry:?}");
    }
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
        Some("destination-rank-proof"),
        "{result:?}"
    );
    let accepted = f.db.get_metadata(&authority_key).await.unwrap().unwrap();
    assert_ne!(accepted, original);
    assert!(!result.stats.selected_work_remaining);
    assert!(!has_deferred_hold("true"));
    let retained = retained_inventory(&f);
    for _ in 0..2 {
        f = reopen_destination_fixture(f).await;
        // Force the obsolete plan due. It must not restart a lookup, inventory
        // or download on a quiet change feed.
        f.db.make_selection_retry_due(super::owner(), "asset-c".into())
            .await
            .unwrap();
        let requests = (
            f.session.calls.load(Ordering::SeqCst),
            f.session.rank_calls.load(Ordering::SeqCst),
        );
        let quiet = baseline_cycle(&f, &config, &session, ordinary_passes(&f)).await;
        assert_eq!(quiet.failed_count, 0, "{quiet:?}");
        assert!(!quiet.stats.selected_work_remaining);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(
            f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
            Some("quiet-zone-cursor")
        );
        assert_eq!(
            f.db.get_metadata(&authority_key).await.unwrap().as_deref(),
            Some(accepted.as_str())
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

fn ordinary_passes(f: &Fixture) -> Vec<AlbumPass> {
    vec![destination_pass(f, "A"), destination_pass(f, "B")]
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

fn retained_inventory(f: &Fixture) -> (i64, i64, i64, i64) {
    f.db.acquire_lock("measuring retained inventory")
        .unwrap()
        .query_row(
            "SELECT (SELECT count(*) FROM provider_active_generations), \
                    (SELECT coalesce(sum(length(body)),0) FROM provider_selection_rank_pages), \
                    (SELECT coalesce(sum(length(manifest)),0) FROM provider_active_decisions), \
                    (SELECT count(*) FROM provider_active_decisions)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
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

#[derive(Clone, Copy)]
enum RankPages {
    Ordinary,
    OnePair,
    RefuseSecond,
}

#[derive(Clone)]
struct PagedSelection(super::CurrentSession, Option<Vec<Value>>, RankPages);

#[async_trait::async_trait]
impl PhotosSession for PagedSelection {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?")
            && let Some(delta) = &self.1
        {
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(
                request["zones"][0]["syncToken"],
                if matches!(self.2, RankPages::OnePair) {
                    "quiet-zone-cursor"
                } else {
                    "paged-selection-proof"
                }
            );
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":delta,"syncToken":"after-new-photo","moreComing":false}]}),
            );
        }
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
            let limit = match self.2 {
                RankPages::RefuseSecond if offset != 0 => {
                    anyhow::bail!("synthetic later-page failure")
                }
                RankPages::RefuseSecond | RankPages::OnePair => limit.min(2),
                RankPages::Ordinary => limit,
            };
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
    install_paged_selection(f, None, RankPages::Ordinary);
}

fn install_paged_selection(f: &mut Fixture, delta: Option<Vec<Value>>, pages: RankPages) {
    f.pass.kind = crate::commands::PassKind::Unfiled;
    f.pass.album = make_captured_cycle_album(
        "PrimarySync",
        f.capture.clone(),
        Box::new(PagedSelection(f.session.clone(), delta, pages)),
    );
}

fn dated_record(name: &str, filename: &str, created: &str, server: &MockServer) -> Vec<Value> {
    let mut record = selected_records(name, server);
    record[0]["fields"]["filenameEnc"]["value"] = json!(filename);
    let created = chrono::DateTime::parse_from_rfc3339(created)
        .unwrap()
        .timestamp_millis();
    record[1]["fields"]["assetDate"]["value"] = json!(created);
    record[1]["fields"]["addedDate"]["value"] = json!(created);
    record
}

async fn first_date_baseline_records(server: &MockServer) -> (Fixture, Vec<Value>, Vec<Value>) {
    let mut f = Fixture::new().await;
    f.db.acquire_lock("date-bounded first baseline without a cursor")
        .unwrap()
        .execute("DELETE FROM metadata WHERE key=?1", [PRIMARY_KEY])
        .unwrap();
    f.config.skip_created_before =
        Some(crate::config::parse_created_date_filter("2024-01-01").unwrap());
    let in_range = dated_record("in", "in-range.jpg", "2024-06-01T12:00:00Z", server);
    let out_of_range = dated_record("old", "out-of-range.jpg", "2023-01-01T12:00:00Z", server);
    (f, in_range, out_of_range)
}

#[tokio::test]
async fn first_date_baseline_drains_to_eof_and_uses_the_change_feed() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/new-photo"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let (mut f, in_range, out_of_range) = first_date_baseline_records(&server).await;
    // The older photo leads the stream, so a prefix-only enumeration would
    // stop before the in-range photo and never reach natural EOF.
    *f.session.records.lock().unwrap() = out_of_range.into_iter().chain(in_range).collect();
    let config = make_run_cycle_config();
    let (_dir, session) = make_shared_session_for_run_cycle().await;
    install_paged_selection(&mut f, None, RankPages::OnePair);
    let first = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
    assert_eq!(
        (first.failed_count, first.stats.downloaded),
        (0, 1),
        "{first:?}"
    );
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
        Some("paged-selection-proof"),
        "the first bounded baseline must establish its checkpoint at real EOF: {first:?}"
    );
    let first_rank_calls = f.session.rank_calls.load(Ordering::SeqCst);
    assert!(
        first_rank_calls >= 3,
        "the baseline must query through the out-of-range page to EOF: {first_rank_calls}"
    );
    let rows = f.db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let original = rows[0].local_path.clone().unwrap();
    assert!(original.ends_with("in-range.JPG"), "{}", original.display());
    let modified = std::fs::metadata(&original).unwrap().modified().unwrap();
    let mut new = dated_record("new", "new.jpg", "2024-07-01T12:00:00Z", &server);
    new[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/new-photo", server.uri()));
    for (delta, downloads, cursor) in [
        (None, 0, "quiet-zone-cursor"),
        (Some(new), 1, "after-new-photo"),
        (None, 0, "quiet-zone-cursor"),
    ] {
        f = reopen_destination_fixture(f).await;
        if let Some(pair) = &delta {
            f.session.records.lock().unwrap().extend(pair.clone());
        }
        install_paged_selection(&mut f, delta, RankPages::OnePair);
        let result = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
        assert_eq!(
            (result.failed_count, result.stats.downloaded),
            (0, downloads),
            "{result:?}"
        );
        assert_eq!(
            f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
            Some(cursor)
        );
        assert_eq!(
            f.session.rank_calls.load(Ordering::SeqCst),
            first_rank_calls
        );
        assert_eq!(std::fs::read(&original).unwrap(), MEDIA);
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            modified
        );
    }
    let files = f.db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(files.len(), 2);
    let added = files
        .into_iter()
        .map(|row| row.local_path.unwrap())
        .find(|p| p.ends_with("new.JPG"))
        .unwrap();
    assert_eq!(std::fs::read(added).unwrap(), MEDIA);
    assert!(!original.with_file_name("out-of-range.JPG").exists());
    server.verify().await;
}

#[tokio::test]
async fn first_date_baseline_keeps_no_checkpoint_when_a_later_page_fails() {
    let server = crate::start_wiremock_or_skip!();
    let (mut f, in_range, out_of_range) = first_date_baseline_records(&server).await;
    *f.session.records.lock().unwrap() = out_of_range.into_iter().chain(in_range).collect();
    let config = make_run_cycle_config();
    let (_dir, session) = make_shared_session_for_run_cycle().await;
    install_paged_selection(&mut f, None, RankPages::RefuseSecond);
    let result = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
    assert!(
        result.stats.enumeration_errors > 0,
        "the later-page failure must be observed: {result:?}"
    );
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap(),
        None,
        "a failed later page cannot establish a first baseline: {result:?}"
    );
    assert!(
        f.db.get_downloaded_page(0, 10).await.unwrap().is_empty(),
        "the failed tail must not download the in-range page it never reached"
    );
}

#[tokio::test]
async fn new_photo_after_baseline_uses_only_the_change_feed() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.db.acquire_lock("no cursor")
        .unwrap()
        .execute("DELETE FROM metadata WHERE key=?1", [PRIMARY_KEY])
        .unwrap();
    let config = make_run_cycle_config();
    let (_dir, session) = make_shared_session_for_run_cycle().await;
    use_paged_selection(&mut f);
    select(&f, "a", &server);
    let first = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
    assert_eq!(
        (first.failed_count, first.stats.downloaded),
        (0, 1),
        "{first:?}"
    );
    let original =
        f.db.get_downloaded_page(0, 1)
            .await
            .unwrap()
            .pop()
            .unwrap()
            .local_path
            .unwrap();
    let modified = std::fs::metadata(&original).unwrap().modified().unwrap();
    f = reopen_destination_fixture(f).await;
    use_paged_selection(&mut f);
    let mut new = selected_records("b", &server);
    new[0]["fields"]["filenameEnc"]["value"] = json!("photo-b.jpg");
    new[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/new-photo", server.uri()));
    f.session.records.lock().unwrap().extend(new.clone());
    f.pass.album = make_captured_cycle_album(
        "PrimarySync",
        f.capture.clone(),
        Box::new(PagedSelection(
            f.session.clone(),
            Some(new.clone()),
            RankPages::Ordinary,
        )),
    );
    let before = f.session.rank_calls.load(Ordering::SeqCst);
    let failed = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
    assert!(failed.failed_count > 0, "{failed:?}");
    assert_eq!(before, f.session.rank_calls.load(Ordering::SeqCst));
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
        Some("paged-selection-proof")
    );
    f = reopen_destination_fixture(f).await;
    use_paged_selection(&mut f);
    f.pass.album = make_captured_cycle_album(
        "PrimarySync",
        f.capture.clone(),
        Box::new(PagedSelection(
            f.session.clone(),
            Some(new),
            RankPages::Ordinary,
        )),
    );
    Mock::given(method("GET"))
        .and(path("/new-photo"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    for (downloads, cursor) in [(1, "after-new-photo"), (0, "quiet-zone-cursor")] {
        let before = f.session.rank_calls.load(Ordering::SeqCst);
        let result = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
        assert_eq!(result.failed_count, 0, "{result:?}");
        assert_eq!(result.stats.downloaded, downloads);
        assert_eq!(
            f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
            Some(cursor)
        );
        assert_eq!(
            before,
            f.session.rank_calls.load(Ordering::SeqCst),
            "{result:?}"
        );
        let rows = f.db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(std::fs::read(row.local_path.unwrap()).unwrap(), MEDIA);
        }
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            modified
        );
        f = reopen_destination_fixture(f).await;
        use_paged_selection(&mut f);
    }
    assert!(
        f.db.inspect_integrity_destination(original.clone())
            .await
            .unwrap()
            .is_some()
    );
    server.verify().await;
    server.reset().await;
    std::fs::remove_file(&original).unwrap();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let restored = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
    assert_eq!(
        (restored.failed_count, restored.stats.downloaded),
        (0, 1),
        "{restored:?}"
    );
    assert_eq!(std::fs::read(original).unwrap(), MEDIA);
    server.verify().await;
}

#[derive(Clone)]
struct AlbumChangeFeed {
    inner: DestinationSelectionSession,
    delta: Option<Vec<Value>>,
    count_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for AlbumChangeFeed {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch?") {
            self.count_calls.fetch_add(1, Ordering::SeqCst);
        }
        if url.contains("/changes/zone?")
            && let Some(records) = &self.delta
        {
            return Ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "records": records,
                    "syncToken": "two-album-new-photo",
                    "moreComing": false
                }]
            }));
        }
        self.inner.post(url, body, headers).await
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn change_feed_pass(
    f: &Fixture,
    name: &str,
    delta: Option<Vec<Value>>,
    count_calls: Arc<AtomicUsize>,
) -> AlbumPass {
    let mut pass = destination_pass_with_session(
        &f.session,
        name,
        Box::new(AlbumChangeFeed {
            inner: DestinationSelectionSession(f.session.clone()),
            delta,
            count_calls,
        }),
    );
    pass.album
        .set_shadow_capture(f.capture.clone(), Arc::from("private"));
    pass
}

async fn clean_two_album_baseline(
    server: &MockServer,
    f: &mut Fixture,
    config: &crate::config::Config,
    session: &crate::auth::SharedSession,
) {
    f.config.folder_structure_albums = Arc::from("{album}");
    f.db.acquire_lock("clean album baseline without a cursor")
        .unwrap()
        .execute("DELETE FROM metadata WHERE key=?1", [PRIMARY_KEY])
        .unwrap();
    select(f, "a", server);
    f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"]["value"] = json!("baseline.jpg");
    let baseline = baseline_cycle(f, config, session, ordinary_passes(f)).await;
    assert_eq!(
        (baseline.failed_count, baseline.stats.downloaded),
        (0, 2),
        "{baseline:?}"
    );
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
        Some("destination-rank-proof"),
        "{baseline:?}"
    );
    for album in ["A", "B"] {
        assert_eq!(
            std::fs::read(f.config.directory.join(album).join("baseline.JPG")).unwrap(),
            MEDIA
        );
    }
}

fn new_photo_delta(server: &MockServer) -> (Vec<Value>, Vec<Value>) {
    let mut new = selected_records("n", server);
    new[0]["fields"]["filenameEnc"]["value"] = json!("new.jpg");
    new[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/new-photo", server.uri()));
    let relations = ["A", "B"]
        .into_iter()
        .map(|album| {
            json!({
                "recordName": format!("asset-n-IN-{album}"),
                "recordType": "CPLContainerRelation",
                "fields": {
                    "containerId": {"value": album},
                    "itemId": {"value": "asset-n"}
                }
            })
        })
        .collect::<Vec<_>>();
    (new, relations)
}

#[tokio::test]
async fn new_photo_in_two_albums_uses_the_change_feed_without_relisting() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/new-photo"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    let config = make_run_cycle_config();
    let (_dir, session) = make_shared_session_for_run_cycle().await;
    clean_two_album_baseline(&server, &mut f, &config, &session).await;

    let (mut pair, relations) = new_photo_delta(&server);
    pair.extend(relations);
    let count_calls = Arc::new(AtomicUsize::new(0));
    let initial_rank = f.session.rank_calls.load(Ordering::SeqCst);
    let directory = f.config.directory.clone();
    let witnesses = |name: &str| {
        ["A", "B"].map(|album| {
            let path = directory.join(album).join(name);
            (
                std::fs::read(&path).unwrap(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
    };
    let baseline = witnesses("baseline.JPG");
    let mut published = None;
    for (cycle, cursor) in [
        "quiet-zone-cursor",
        "quiet-zone-cursor",
        "two-album-new-photo",
        "quiet-zone-cursor",
    ]
    .into_iter()
    .enumerate()
    {
        f = reopen_destination_fixture(f).await;
        if cycle == 1 {
            f.session.records.lock().unwrap().extend(pair.clone());
            f.db.acquire_lock("refuse only the new cursor").unwrap().execute_batch("CREATE TRIGGER refuse_album_cursor BEFORE INSERT ON metadata WHEN NEW.key='sync_token:PrimarySync' AND NEW.value='two-album-new-photo' BEGIN SELECT RAISE(ABORT,'test cursor fault'); END;").unwrap();
        } else if cycle == 2 {
            f.db.acquire_lock("allow cursor retry")
                .unwrap()
                .execute_batch("DROP TRIGGER refuse_album_cursor")
                .unwrap();
        }
        let delta = matches!(cycle, 1 | 2).then(|| pair.clone());
        let passes = vec![
            change_feed_pass(&f, "A", delta, count_calls.clone()),
            change_feed_pass(&f, "B", None, count_calls.clone()),
        ];
        f.session.lookup_names.lock().unwrap().clear();
        let result = baseline_cycle(&f, &config, &session, passes).await;
        if cycle == 1 {
            assert!(!result.db_sync_token_advance_safe, "{result:?}");
            let lookups = f.session.lookup_names.lock().unwrap();
            for name in ["asset-n-IN-A", "asset-n-IN-B"] {
                assert!(lookups.iter().any(|n| n == name), "{lookups:?}");
            }
        } else {
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(
                f.session.lookup_names.lock().unwrap().is_empty(),
                "cycle {cycle}: {result:?}"
            );
        }
        assert_eq!(
            result.stats.downloaded,
            if cycle == 1 { 2 } else { 0 },
            "{result:?}"
        );
        assert_eq!(
            f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
            Some(cursor),
            "{result:?}"
        );
        assert_eq!(
            f.session.rank_calls.load(Ordering::SeqCst),
            initial_rank,
            "{result:?}"
        );
        assert_eq!(count_calls.load(Ordering::SeqCst), 0, "{result:?}");
        assert_eq!(witnesses("baseline.JPG"), baseline);
        if cycle > 0 {
            let new = witnesses("new.JPG");
            assert!(new.iter().all(|(bytes, _)| bytes == MEDIA));
            assert_eq!(published.get_or_insert_with(|| new.clone()), &new);
            let batches: i64 = f.db.acquire_lock("check exact batch replay").unwrap().query_row("SELECT count(*) FROM provider_active_generations WHERE sealed=1 AND json_extract(specification,'$.profile.coverage')='library_delta'", [], |r|r.get(0)).unwrap();
            assert_eq!(batches, 1);
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn captured_album_link_with_mismatched_current_container_holds_and_recovers() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/new-photo"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(2)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    let config = make_run_cycle_config();
    let (_dir, session) = make_shared_session_for_run_cycle().await;
    clean_two_album_baseline(&server, &mut f, &config, &session).await;

    f = reopen_destination_fixture(f).await;
    let (new, relations) = new_photo_delta(&server);
    let mut delta = new.clone();
    delta.extend(relations.clone());
    // The captured relation says B, but the current lookup returns the same
    // link record pointing at another container. B must not publish from that.
    let mut lookup_relations = relations;
    lookup_relations[1]["fields"]["containerId"]["value"] = json!("C");
    {
        let mut records = f.session.records.lock().unwrap();
        records.extend(new);
        records.extend(lookup_relations);
    }
    let count_calls = Arc::new(AtomicUsize::new(0));
    let held_passes = vec![
        change_feed_pass(&f, "A", Some(delta.clone()), count_calls.clone()),
        change_feed_pass(&f, "B", None, count_calls.clone()),
    ];
    let rank_before = f.session.rank_calls.load(Ordering::SeqCst);
    let count_before = count_calls.load(Ordering::SeqCst);
    f.session.lookup_names.lock().unwrap().clear();
    let held = baseline_cycle(&f, &config, &session, held_passes).await;
    assert_eq!(
        std::fs::read(f.config.directory.join("A/new.JPG")).unwrap(),
        MEDIA
    );
    assert!(!f.config.directory.join("B/new.JPG").exists(), "{held:?}");
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
        Some("destination-rank-proof"),
        "{held:?}"
    );
    assert!(held.stats.selected_work_remaining, "{held:?}");
    let lookups = f.session.lookup_names.lock().unwrap().clone();
    for record in ["asset-n", "n", "asset-n-IN-B"] {
        assert!(lookups.iter().any(|name| name == record), "{lookups:?}");
    }
    assert_eq!(
        rank_before,
        f.session.rank_calls.load(Ordering::SeqCst),
        "{held:?}"
    );
    assert_eq!(count_before, count_calls.load(Ordering::SeqCst), "{held:?}");

    f = reopen_destination_fixture(f).await;
    {
        let mut records = f.session.records.lock().unwrap();
        let link = records
            .iter_mut()
            .find(|record| record["recordName"] == "asset-n-IN-B")
            .unwrap();
        link["fields"]["containerId"]["value"] = json!("B");
    }
    let a = f.config.directory.join("A/new.JPG");
    let a_bytes = std::fs::read(&a).unwrap();
    let a_modified = std::fs::metadata(&a).unwrap().modified().unwrap();
    f.db.make_selection_retry_due(super::owner(), "asset-n".into())
        .await
        .unwrap();
    let repaired = baseline_cycle(
        &f,
        &config,
        &session,
        vec![
            change_feed_pass(&f, "A", Some(delta), Arc::new(AtomicUsize::new(0))),
            change_feed_pass(&f, "B", None, Arc::new(AtomicUsize::new(0))),
        ],
    )
    .await;
    assert_eq!(repaired.failed_count, 0, "{repaired:?}");
    assert_eq!(repaired.stats.downloaded, 1, "{repaired:?}");
    assert_eq!(
        std::fs::read(f.config.directory.join("B/new.JPG")).unwrap(),
        MEDIA,
        "{repaired:?}"
    );
    assert_eq!(std::fs::read(&a).unwrap(), a_bytes);
    assert_eq!(
        std::fs::metadata(&a).unwrap().modified().unwrap(),
        a_modified
    );
    assert_eq!(
        f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
        Some("two-album-new-photo"),
        "{repaired:?}"
    );
    for album in ["A", "B"] {
        assert_eq!(
            std::fs::read(f.config.directory.join(album).join("baseline.JPG")).unwrap(),
            MEDIA
        );
    }
    server.verify().await;
}

#[tokio::test]
async fn full_sync_410_keeps_healthy_transfers_running_before_cleanup() {
    check_expired_transfer_at_first_baseline(MEDIA).await;
}

#[tokio::test]
async fn changed_unpublished_rendition_establishes_first_baseline_after_410() {
    check_expired_transfer_at_first_baseline(include_bytes!(
        "../../../../../tests/data/media/pattern.jpg"
    ))
    .await;
}

async fn check_expired_transfer_at_first_baseline(current: &'static [u8]) {
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
    if current != MEDIA {
        use crate::state::DownloadStateStore;
        let mut unfinished = f.db.get_failed().await.unwrap();
        unfinished.extend(f.db.get_pending().await.unwrap());
        let mut unconfirmed = unfinished
            .into_iter()
            .find(|r| r.id.as_ref() == "asset-expired")
            .unwrap();
        unconfirmed.checksum = data_encoding::BASE64
            .encode(&Sha256::digest(current))
            .into();
        assert!(matches!(
            f.db.upsert_seen(&unconfirmed).await,
            Err(crate::state::error::StateError::ProviderWorkConflict(
                "unfinished_asset_changed"
            ))
        ));
    }
    {
        let mut records = f.session.records.lock().unwrap();
        records[1]["fields"]["captionEnc"]["value"] = json!("New caption");
        let resource = &mut records[0]["fields"]["resOriginalRes"]["value"];
        resource["fileChecksum"] = json!(data_encoding::BASE64.encode(&Sha256::digest(current)));
        resource["size"] = json!(current.len());
    }
    let updated = f.session.records.lock().unwrap().clone();
    f.capture(updated, "updated-caption").await;
    Mock::given(method("GET"))
        .and(path("/expired"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(current))
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
        assert_eq!(
            f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
            Some(if expected_downloads == 1 {
                "paged-selection-proof"
            } else {
                "quiet-zone-cursor"
            })
        );
        let rows = f.db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            let expected = if row.id.as_ref() == "asset-expired" {
                current
            } else {
                MEDIA
            };
            assert_eq!(std::fs::read(row.local_path.unwrap()).unwrap(), expected);
        }
        if current != MEDIA {
            let old: (bool, Option<String>) = f.db.acquire_lock("prior rendition remains unpublished").unwrap().query_row(
                "SELECT verified_media,source_checksum FROM provider_active_destinations WHERE child='asset-expired' AND checksum=?1",
                [data_encoding::BASE64.encode(&Sha256::digest(MEDIA))], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            assert_eq!(old, (false, None));
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn first_checkpoint_after_indexing_needs_no_replay_without_a_prior_cursor() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    use_paged_selection(&mut f);
    f.db.acquire_lock("no cursor before indexing")
        .unwrap()
        .execute("DELETE FROM metadata WHERE key=?1", [PRIMARY_KEY])
        .unwrap();
    select(&f, "old", &server);
    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
        json!(format!("{}/expired", server.uri()));
    let config = make_run_cycle_config();
    let (_dir, session) = make_shared_session_for_run_cycle().await;
    let first = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
    assert!(first.stats.selected_work_remaining);
    assert!(f.db.get_metadata(PRIMARY_KEY).await.unwrap().is_none());
    let replay = crate::sync_cycle::indexing_replay_key("PrimarySync");
    f.db.set_metadata(&replay, "1").await.unwrap();
    let mut peer = selected_records("peer", &server);
    peer[0]["fields"]["filenameEnc"]["value"] = json!("peer.jpg");
    *f.session.records.lock().unwrap() = peer.clone();
    f.capture(peer, "peer-observation").await;
    for (downloaded, cursor) in [
        (1, "paged-selection-proof"),
        (0, "quiet-zone-cursor"),
        (0, "quiet-zone-cursor"),
    ] {
        f = reopen_destination_fixture(f).await;
        use_paged_selection(&mut f);
        let before = f.session.rank_calls.load(Ordering::SeqCst);
        let result = baseline_cycle(&f, &config, &session, vec![f.pass.clone()]).await;
        assert_eq!(
            (result.failed_count, result.stats.downloaded),
            (0, downloaded),
            "{result:?}"
        );
        assert_eq!(
            f.db.get_metadata(PRIMARY_KEY).await.unwrap().as_deref(),
            Some(cursor)
        );
        assert_eq!(f.db.get_metadata(&replay).await.unwrap(), None);
        if downloaded == 0 {
            assert_eq!(before, f.session.rank_calls.load(Ordering::SeqCst));
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
    assert!(!current.checkpoint.sync_token_blocked, "{current:?}");
    assert_eq!(
        current.stats.selected_work_remaining,
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
        assert_eq!(
            result.sync_token.as_deref(),
            Some("quiet-zone-cursor"),
            "{result:?}"
        );
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
        let (_, rank_bytes, decision_bytes, _) = retained_inventory(&f);
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
                f.db.selection_debt(super::owner(), scope.clone(), hash.clone(), false, None)
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

#[cfg(feature = "xmp")]
#[tokio::test]
async fn selection_seal_grouping_drift_defers_only_the_current_cycle() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.pass = destination_pass(&f, "A");
    f.config.folder_structure_albums = Arc::from("{album}");
    f.config.metadata.xmp_sidecar = true;
    f.config.post_sync_metadata = true;
    select(&f, "m", &server);
    f.db.acquire_lock("interrupt first selection seal after publication")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER interrupt_selection_seal BEFORE UPDATE OF sealed ON provider_active_generations
             WHEN NEW.sealed=1 BEGIN SELECT RAISE(ABORT,'synthetic selection seal interruption'); END;",
        )
        .unwrap();
    let interrupted = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        interrupted
            .to_string()
            .contains("synthetic selection seal interruption"),
        "{interrupted:?}"
    );
    f.db.acquire_lock("remove synthetic seal interruption")
        .unwrap()
        .execute_batch("DROP TRIGGER interrupt_selection_seal")
        .unwrap();
    let (old, scope, config_hash): (String, String, String) =
        f.db.acquire_lock("first interrupted generation")
            .unwrap()
            .query_row(
                "SELECT id,scope,config_hash FROM provider_active_generations WHERE sealed=0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    let obligation = |f: &Fixture| {
        f.db.acquire_lock("frozen selection obligation")
            .unwrap()
            .prepare(
                "SELECT grouping_hash,intent_hash,path,verified_media
                 FROM provider_active_destinations WHERE generation=?1 ORDER BY pass_key,path",
            )
            .unwrap()
            .query_map([&old], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };

    f = reopen_destination_fixture(f).await;
    // Freeze captures this live grouping before dispatch. A controlled live
    // grouping change during inventory then makes the stored intent stale.
    f.db.acquire_lock("change live grouping during inventory")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER drift_selection_grouping AFTER UPDATE OF status ON album_membership_snapshots
             WHEN NEW.status='complete' BEGIN
                 INSERT OR IGNORE INTO asset_albums(library,asset_id,album_name,source)
                 VALUES('PrimarySync','asset-m','drift','icloud');
             END;",
        )
        .unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let refused = cycle(&f).await;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests,
        "a refused seal must not fetch media"
    );
    assert_eq!(refused.stats.state_write_failures, 1, "{refused:?}");
    assert_eq!(refused.checkpoint.state_write_failures, 1, "{refused:?}");
    assert!(matches!(
        refused.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert!(refused.sync_token.is_none(), "{refused:?}");
    assert!(refused.checkpoint.sync_token_blocked);
    assert!(refused.checkpoint.scanned_generation.is_none());
    assert_eq!(refused.stats.sync_token_receivers_with_token, Some(1));
    assert_eq!(refused.stats.sync_token_unique_values, Some(1));
    assert_eq!(
        refused.stats.sync_token_blocked_reason,
        Some("selection_generation_deferred")
    );
    let frozen = obligation(&f);
    assert_eq!(frozen.len(), 1);
    assert!(
        frozen
            .iter()
            .all(|(grouping, intent, _, media)| grouping.is_some() && intent.is_some() && *media)
    );
    let unsealed = |f: &Fixture| {
        f.db.acquire_lock("failed seal leaves the root unsealed")
            .unwrap()
            .query_row(
                "SELECT sealed=0 AND seal_hash IS NULL FROM provider_active_generations WHERE id=?1",
                [&old],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    };
    assert!(unsealed(&f));
    let media: Vec<_> = frozen
        .iter()
        .map(|(_, _, path, _)| {
            serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(path)
                .unwrap()
                .to_path()
        })
        .collect();
    let published: Vec<_> = media
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert_eq!(published, [MEDIA]);

    f.db.acquire_lock("restore normal grouping evidence")
        .unwrap()
        .execute_batch(
            "DROP TRIGGER drift_selection_grouping;
             DELETE FROM asset_albums WHERE library='PrimarySync' AND asset_id='asset-m' AND album_name='drift';",
        )
        .unwrap();
    f = reopen_destination_fixture(f).await;
    let next = cycle(&f).await;
    assert!(next.sync_token.is_some(), "{next:?}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests,
        "retry reuses the published media"
    );
    let current =
        f.db.current_selection_generation(super::owner(), scope, config_hash)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(current.id, old, "{next:?}");
    assert!(current.sealed);
    assert_eq!(obligation(&f), frozen);
    for (path, bytes) in media.iter().zip(&published) {
        assert_eq!(&std::fs::read(path).unwrap(), bytes);
    }
    server.verify().await;
}
