//! Focused repeated source observation and changed-URL controls.
use super::{ALBUM, CrossZoneSession, MEDIA, OWNER, SOURCE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn issue845_album(
    session: &dyn PhotosSession,
    name: &str,
    capture: &crate::icloud::photos::inbox::ShadowCapture,
) -> PhotoAlbum {
    let database = if name == OWNER { "private" } else { "shared" };
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: format!(
                "https://synthetic.example/database/1/com.apple.photos.cloud/production/{database}"
            )
            .into(),
            name: "Selected".into(),
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(zone(name)),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: if name == OWNER {
                Some(ALBUM.into())
            } else {
                None
            },
            cross_zone_sources: Vec::new(),
        },
        session.clone_box(),
    );
    album.set_shadow_capture(capture.clone(), database.into());
    album
}

#[derive(Clone)]
struct Issue845Session(
    CrossZoneSession,
    bool,
    Option<(Arc<std::sync::atomic::AtomicUsize>, String)>,
    Arc<Mutex<Vec<&'static str>>>,
    bool,
    bool,
);

#[async_trait::async_trait]
impl PhotosSession for Issue845Session {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(SOURCE),"syncToken":"boundary","deleted":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            let count = self
                .0
                .records_for(SOURCE)
                .iter()
                .filter(|record| record["recordType"] == "CPLAsset")
                .count() as u64;
            let request: Value = serde_json::from_str(&body)?;
            self.0.requests.lock().unwrap().push((url.into(), request));
            return Ok(album_count_response(count));
        }
        if url.contains("/changes/zone?") {
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(request["zones"][0]["zoneID"], zone(SOURCE));
            let expected = match self.0.cycle {
                0 => "boundary".to_owned(),
                2 if self.5 => "current-0".to_owned(),
                cycle => format!("current-{}", cycle - 1),
            };
            assert_eq!(request["zones"][0]["syncToken"], expected);
            self.0.requests.lock().unwrap().push((url.into(), request));
            let records = if self.0.cycle == 1 || (self.0.cycle == 2 && self.1) {
                self.0.records_for(SOURCE)
            } else {
                Vec::new()
            };
            return Ok(json!({"zones":[{"zoneID":zone(SOURCE),"records":records,
                "syncToken":format!("current-{}", self.0.cycle),"moreComing":false}]}));
        }
        if let Some((movie_gets, fresh_url)) = &self.2
            && url.contains("/records/lookup?")
            && movie_gets.load(std::sync::atomic::Ordering::SeqCst) > 0
        {
            let requested: Value = serde_json::from_str(&body)?;
            assert_eq!(requested["zoneID"], zone(SOURCE));
            let names = requested["records"]
                .as_array()
                .expect("scoped lookup names");
            if names.iter().any(|r| r["recordName"] == "asset-shared") {
                let paired = names.iter().any(|r| r["recordName"] == "shared");
                self.3.lock().unwrap().push(if paired {
                    "lookup:pair"
                } else {
                    "lookup:child"
                });
            }
            let mut records = self.0.records.as_ref().clone();
            if let Some(master) = records.iter_mut().find(|record| {
                record["recordName"] == "shared" && record["recordType"] == "CPLMaster"
            }) {
                master["fields"]["resOriginalVidComplRes"]["value"]["downloadURL"] =
                    json!(fresh_url);
            }
            let mut current = self.0.clone();
            current.records = Arc::new(records);
            return current.post(url, body, headers).await;
        }
        self.0.post(url, body, headers).await
    }
    async fn current_source_authority(
        &self,
    ) -> Option<crate::icloud::photos::session::SourceAuthority> {
        crate::icloud::photos::session::SourceAuthority::synthetic(
            &baseline_owner(),
            "https://synthetic.example",
        )
    }
    async fn post_changes_with_evidence(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
        invocation: crate::icloud::photos::session::InvocationToken,
    ) -> anyhow::Result<crate::icloud::photos::session::ChangesResponse> {
        // These are explicitly synthetic fixture bytes, generated from the
        // known test response, not proof about a Value-only production adapter.
        let response = self.post(url, body.clone(), headers).await?;
        let raw_fixture = serde_json::to_vec(&response)?;
        if self.4 && url.contains("/records/lookup?") {
            return Ok(crate::icloud::photos::session::ChangesResponse {
                body: raw_fixture,
                original: None,
                accepted: None,
            });
        }
        crate::icloud::photos::session::ChangesResponse::synthetic(
            &baseline_owner(),
            "https://synthetic.example",
            url,
            &body,
            headers,
            &raw_fixture,
            invocation,
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn issue845_value_only_session_cannot_certify_source_bytes() {
    let session = CrossZoneSession {
        cycle: 0,
        records: Arc::default(),
        requests: Arc::default(),
    };
    assert!(session.current_source_authority().await.is_none());
    let response = crate::icloud::photos::session::retry_post_changes_with_evidence(
        &session,
        "https://synthetic.example/zones/list",
        "{}",
        &[],
        &crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    assert!(
        response.original.is_none(),
        "a Value-only session must not certify original source bytes"
    );
    assert!(
        response.accepted.is_none(),
        "a Value-only response has no accepted source invocation"
    );
    assert!(
        serde_json::from_slice::<Value>(&response.body)
            .unwrap()
            .get("zones")
            .is_some()
    );
}

#[tokio::test]
async fn issue845_identical_observation_shares_future_transfer_wait() {
    issue845_partial_movie_case(true, true, false, false).await;
}

#[tokio::test]
async fn issue845_authenticated_quiet_reopen_preserves_exact_movie_wait() {
    issue845_partial_movie_case(false, true, false, false).await;
}

#[tokio::test]
async fn issue845_changed_movie_url_is_evaluated_before_old_deadline() {
    issue845_partial_movie_case(true, false, true, false).await;
}

#[tokio::test]
async fn issue845_same_generation_refreshed_url_is_evaluated_independently() {
    issue845_partial_movie_case_inner(false, false, false, false, false, Some(0), false).await;
}

#[tokio::test]
async fn issue845_selected_refresh_witness_held_before_get() {
    issue845_partial_movie_case_inner(false, false, false, false, false, Some(1), false).await;
}

#[tokio::test]
async fn issue845_refreshed_410_ticket_survives_state_reopen() {
    issue845_partial_movie_case_inner(false, false, false, false, false, Some(2), false).await;
}

#[tokio::test]
async fn issue845_refreshed_503_is_accounted_once_and_reopens_with_independent_progress() {
    issue845_partial_movie_case_inner(false, false, false, false, false, Some(3), false).await;
}

#[tokio::test]
async fn issue845_previously_dispatched_sibling_progresses_before_movie_deadline() {
    issue845_partial_movie_case_inner(false, false, false, false, false, None, true).await;
}

#[tokio::test]
async fn issue845_aborted_410_waiter_keeps_guard_until_real_retry_commit() {
    issue845_partial_movie_case_inner(true, true, false, false, true, None, false).await;
}

async fn issue845_partial_movie_case(
    repeated_observation: bool,
    approved_wait: bool,
    changed_url: bool,
    changed_resource: bool,
) {
    issue845_partial_movie_case_inner(
        repeated_observation,
        approved_wait,
        changed_url,
        changed_resource,
        false,
        None,
        false,
    )
    .await;
}

async fn issue845_partial_movie_case_inner(
    repeated_observation: bool,
    approved_wait: bool,
    changed_url: bool,
    changed_resource: bool,
    abort_410_witness: bool,
    refresh_same_generation: Option<u8>,
    previously_dispatched_sibling: bool,
) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let server = crate::test_helpers::start_wiremock_or_skip("issue845 partial live")
        .await
        .expect("synthetic CDN must bind");
    let occupant =
        include_bytes!("../../../../../../tests/data/media/icloud-still.heic").as_slice();
    let still = include_bytes!("../../../../../../tests/data/media/apple-live.heic").as_slice();
    let movie = include_bytes!("../../../../../../tests/data/media/apple-live.mov").as_slice();
    let mut changed_movie = movie.to_vec();
    changed_movie.extend_from_slice(b"synthetic-changed-resource");
    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let still_gets = Arc::new(AtomicUsize::new(0));
    let movie_gets = Arc::new(AtomicUsize::new(0));
    let independent_gets = Arc::new(AtomicUsize::new(0));
    let fresh_gets = Arc::new(AtomicUsize::new(0));
    let changed_gets = Arc::new(AtomicUsize::new(0));
    let changed_counter = changed_gets.clone();
    Mock::given(method("GET"))
        .and(path("/changed-movie"))
        .respond_with(move |_: &wiremock::Request| {
            changed_counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(410)
        })
        .mount(&server)
        .await;
    let fresh_counter = fresh_gets.clone();
    let fresh_events = events.clone();
    Mock::given(method("GET"))
        .and(path("/fresh-movie"))
        .respond_with(move |_: &wiremock::Request| {
            fresh_counter.fetch_add(1, Ordering::SeqCst);
            if refresh_same_generation == Some(3) {
                fresh_events.lock().unwrap().push("fresh:503");
                ResponseTemplate::new(503)
            } else if refresh_same_generation == Some(2) {
                fresh_events.lock().unwrap().push("fresh:410");
                ResponseTemplate::new(410)
            } else if refresh_same_generation.is_some() {
                fresh_events.lock().unwrap().push("movie:200");
                ResponseTemplate::new(200).set_body_bytes(movie)
            } else {
                ResponseTemplate::new(410)
            }
        })
        .mount(&server)
        .await;
    let independent_counter = independent_gets.clone();
    Mock::given(method("GET"))
        .and(path("/independent"))
        .respond_with(move |_: &wiremock::Request| {
            independent_counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_bytes(MEDIA)
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/occupant"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(occupant))
        .expect(1)
        .mount(&server)
        .await;
    let still_ready = Arc::new(std::sync::atomic::AtomicBool::new(
        !previously_dispatched_sibling,
    ));
    let ready = still_ready.clone();
    let still_events = events.clone();
    let count = still_gets.clone();
    Mock::given(method("GET"))
        .and(path("/still"))
        .respond_with(move |_: &wiremock::Request| {
            count.fetch_add(1, Ordering::SeqCst);
            if ready.load(Ordering::SeqCst) {
                if previously_dispatched_sibling {
                    still_events.lock().unwrap().push("still:200");
                }
                ResponseTemplate::new(200).set_body_bytes(still)
            } else {
                still_events.lock().unwrap().push("still:503");
                ResponseTemplate::new(503)
            }
        })
        .mount(&server)
        .await;
    let count = movie_gets.clone();
    let movie_events = events.clone();
    Mock::given(method("GET"))
        .and(path("/expired-movie"))
        .respond_with(move |_: &wiremock::Request| {
            count.fetch_add(1, Ordering::SeqCst);
            if refresh_same_generation.is_some() || previously_dispatched_sibling {
                movie_events.lock().unwrap().push("movie:410");
            }
            ResponseTemplate::new(410)
        })
        .mount(&server)
        .await;

    // An explicit diagnostic directory retains only this synthetic fixture,
    // including on panic; without it the test keeps ordinary TempDir cleanup.
    let diagnostic_parent = std::env::var_os("KEI_ISSUE845_DIAGNOSTIC_DIR");
    let dir = if let Some(parent) = &diagnostic_parent {
        std::fs::create_dir_all(parent).unwrap();
        tempfile::Builder::new()
            .prefix("issue845-")
            .tempdir_in(parent)
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    let fixture = dir.path().to_path_buf();
    let _fixture_guard = if diagnostic_parent.is_some() {
        let retained = dir.keep();
        eprintln!("ISSUE845_DIAGNOSTIC_FIXTURE path={}", retained.display());
        None
    } else {
        Some(dir)
    };
    let root = fixture.join("photos");
    std::fs::create_dir(&root).unwrap();
    let database = fixture.join("state.db");
    let date = crate::sync_loop::test_support::run_cycle_expected_date_dir();
    let first_path = root.join(&date).join("collision.HEIC");
    let still_path = root
        .join(&date)
        .join(format!("collision-{}.HEIC", still.len()));
    let movie_path = root
        .join(&date)
        .join(format!("collision-{}_HEVC.MOV", still.len()));
    let _timing = crate::download::planner::distinct_timing_test_hook::Control::install_positive(
        first_path.clone(),
        still_path.clone(),
    );
    let owner = baseline_owner();
    let (account, provider) = owner.authenticated_keys().unwrap();
    let mut records = Vec::new();
    for (id, endpoint, bytes) in [("owner", "occupant", occupant), ("shared", "still", still)] {
        let mut current = pair(
            id,
            "collision.HEIC",
            &format!("{}/{endpoint}", server.uri()),
            22,
        );
        current[0]["fields"]["itemType"] = json!({"value":"public.heic"});
        current[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
        current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(bytes.len());
        current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)));
        if id == "shared" {
            current[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
                "downloadURL":format!("{}/expired-movie", server.uri()), "size":movie.len(),
                "fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
            }});
            current[0]["fields"]["resOriginalVidComplFileType"] =
                json!({"value":"com.apple.quicktime-movie"});
        }
        for record in &mut current {
            record["zoneID"] = zone(SOURCE);
            if record["recordType"] == "CPLAsset" {
                record["fields"]["masterRef"]["value"]["zoneID"] = zone(SOURCE);
            }
        }
        records.extend(current);
    }
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let expected_scope = json!({
        "zone":zone(SOURCE), "name":"Selected",
        "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
        "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
        "query_filter":null, "container_id":null, "cross_zone_sources":[]
    });
    let expected_pass = format!(
        "{:x}",
        Sha256::digest(
            json!({"kind":"Unfiled","scope":expected_scope,"excludes":[]})
                .to_string()
                .as_bytes()
        )
    );
    let mut saved_receipt = None;
    let mut saved_stamp = None;
    let mut occupant_stamp = None;
    let mut saved_retry: Option<Value> = None;
    let mut original_equivalence: Option<Value> = None;
    let mut linked_generation: Option<String> = None;
    let mut independent_stamp = None;
    let mut test_clock: Option<crate::state::db::retry_clock::test_clock::Control> = None;
    let mut saved_component_epoch: Option<String> = None;
    let mut initial_refresh_wait: Option<(String, i64, String, String)> = None;
    let mut initial_movie_epoch: Option<String> = None;
    let mut completed_movie_stamp = None;
    let mut ticket_evidence: Option<(String, String, String)> = None;
    let mut ticket_outcome: Option<String> = None;
    let mut retained_still: Option<(String, std::time::SystemTime)> = None;
    let mut non410_before_attempts: Option<i64> = None;
    let mut non410_terminal: Option<String> = None;
    let mut non410_still: Option<(String, std::time::SystemTime)> = None;
    let cycles: usize = if repeated_observation && !approved_wait {
        2
    } else {
        3
    };
    // A real empty inventory and its changes bridge establish the cursor.
    // Later observations are solely from authenticated changes pages.
    for iteration in 0..=cycles {
        let baseline = iteration == 0;
        let cycle = iteration.saturating_sub(1);
        let db = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
        if !baseline && cycle == 1 && refresh_same_generation == Some(2) {
            let conn = db
                .acquire_lock("reopened actual refreshed 410 ticket")
                .unwrap();
            let (ticket, proof, generation) = ticket_evidence.as_ref().unwrap();
            let row: String = conn.query_row("SELECT json_array(id,proof_hash,generation,phase,attempt,deadline,outcome_proof) FROM provider_refreshed_transfer WHERE id=?1",[ticket],|r|r.get(0)).unwrap();
            assert_eq!(&row, ticket_outcome.as_ref().unwrap());
            let count:i64=conn.query_row("SELECT count(*) FROM provider_refreshed_transfer WHERE id=?1 AND proof_hash=?2 AND generation=?3 AND phase='expired'",rusqlite::params![ticket,proof,generation],|r|r.get(0)).unwrap();
            assert_eq!(count, 1);
            assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
            assert!(!movie_path.exists());
            let receipt:String=conn.query_row("SELECT json_array(account_key,provider_key,scope,library,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,proof_hash) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='original'",[SOURCE],|r|r.get(0)).unwrap();
            assert_eq!(receipt, retained_still.as_ref().unwrap().0);
            assert_eq!(
                std::fs::metadata(&still_path).unwrap().modified().unwrap(),
                retained_still.as_ref().unwrap().1
            );
            eprintln!(
                "ISSUE845_REFRESH_410_REOPEN_REACHED ticket=expired unchanged=true reopened_only=true"
            );
            return;
        }
        if !baseline && cycle == 1 && refresh_same_generation == Some(3) {
            let conn = db.acquire_lock("reopened selected 503 terminal").unwrap();
            let (ticket, proof, generation) = ticket_evidence.as_ref().unwrap();
            let terminal:String=conn.query_row("SELECT json_array(id,proof_hash,generation,phase,failure_status,failure_error_hash,outcome_proof) FROM provider_refreshed_transfer WHERE id=?1",[ticket],|r|r.get(0)).unwrap();
            assert_eq!(&terminal, non410_terminal.as_ref().unwrap());
            let count:i64=conn.query_row("SELECT count(*) FROM provider_refreshed_transfer WHERE id=?1 AND proof_hash=?2 AND generation=?3 AND phase='failed' AND failure_status=503",rusqlite::params![ticket,proof,generation],|r|r.get(0)).unwrap();
            assert_eq!(count, 1);
            let attempts:i64=conn.query_row("SELECT download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap();
            assert_eq!(
                attempts,
                non410_before_attempts.unwrap() + 1,
                "reopen does not double-finalize the actual 503"
            );
            let receipt:String=conn.query_row("SELECT json_array(account_key,provider_key,scope,library,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,proof_hash) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='original'",[SOURCE],|r|r.get(0)).unwrap();
            assert_eq!(receipt, non410_still.as_ref().unwrap().0);
            assert_eq!(
                std::fs::metadata(&still_path).unwrap().modified().unwrap(),
                non410_still.as_ref().unwrap().1
            );
            assert_eq!(movie_gets.load(Ordering::SeqCst), 1);
            assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
            eprintln!(
                "ISSUE845_REFRESH_503_REOPEN_REACHED terminal=failed attempts_increment=1 independent_receipt=1 scheduler_not_run=true"
            );
            return;
        }
        let mut cycle_records = if baseline {
            Vec::new()
        } else {
            records.clone()
        };
        if !baseline && (changed_url || changed_resource) && cycle == 1 {
            let master = cycle_records
                .iter_mut()
                .find(|record| {
                    record["recordType"] == "CPLMaster" && record["recordName"] == "shared"
                })
                .unwrap();
            master["fields"]["resOriginalVidComplRes"]["value"]["downloadURL"] = json!(format!(
                "{}/{}",
                server.uri(),
                if changed_url {
                    "fresh-movie"
                } else {
                    "changed-movie"
                }
            ));
            if changed_resource {
                master["fields"]["resOriginalVidComplRes"]["value"]["fileChecksum"] = json!(
                    base64::engine::general_purpose::STANDARD
                        .encode(Sha256::digest(&changed_movie))
                );
                master["fields"]["resOriginalVidComplRes"]["value"]["size"] =
                    json!(changed_movie.len());
            }
        }
        if !baseline
            && ((approved_wait && repeated_observation && cycle >= 1)
                || (refresh_same_generation == Some(3) && cycle == 0))
        {
            let mut current = pair(
                "independent",
                "independent.JPG",
                &format!("{}/independent", server.uri()),
                23,
            );
            current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MEDIA)));
            for record in &mut current {
                record["zoneID"] = zone(SOURCE);
                if record["recordType"] == "CPLAsset" {
                    record["fields"]["masterRef"]["value"]["zoneID"] = zone(SOURCE);
                }
            }
            cycle_records.extend(current);
        }
        let session = Issue845Session(
            CrossZoneSession {
                cycle: if baseline { 0 } else { cycle + 1 },
                records: Arc::new(cycle_records),
                requests: Arc::default(),
            },
            repeated_observation,
            refresh_same_generation
                .is_some()
                .then(|| (movie_gets.clone(), format!("{}/fresh-movie", server.uri()))),
            events.clone(),
            refresh_same_generation == Some(1),
            approved_wait && repeated_observation && !changed_url && !changed_resource,
        );
        let provider_requests = session.0.requests.clone();
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com");
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: issue845_album(&session, SOURCE, &capture),
            exclude_ids: Arc::default(),
        };
        assert_eq!(pass.album.selection_scope(), expected_scope);
        if !baseline && cycle == 2 {
            // Advance only this authenticated retained obligation, after a quiet
            // reopen has proved that the original deadline was respected.
            if approved_wait {
                let deadline = original_equivalence.as_ref().unwrap()[12].as_i64().unwrap();
                test_clock
                    .as_ref()
                    .expect("clock installed after the empty baseline")
                    .set(deadline + 1);
            } else {
                db.make_selection_retry_due(owner.clone(), "asset-shared".into())
                    .await
                    .unwrap();
            }
        }
        let before_requests = movie_gets.load(Ordering::SeqCst);
        let before_still_requests = still_gets.load(Ordering::SeqCst);
        let before_time = crate::state::db::retry_clock::now_for_path(&database);
        let mut selected =
            make_run_cycle_library_state_with_passes(SOURCE, &sync_token_key(SOURCE), vec![pass]);
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/com.apple.photos.cloud/production/shared".into(),
            Arc::default(),
            Box::new(session),
            Arc::new(zone(SOURCE)),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(capture);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, excludes, groups, library| {
            let mut config = base(mode, excludes, groups, library);
            Arc::make_mut(&mut config).retry.max_retries = 0;
            assert_eq!(
                config.retry.max_retries, 0,
                "each actual request uses zero per-transfer retries"
            );
            if refresh_same_generation.is_some() || previously_dispatched_sibling {
                Arc::make_mut(&mut config).concurrent_downloads = 1;
            }
            config
        };
        if !baseline && abort_410_witness {
            use futures_util::future::{AbortHandle, Abortable};
            let (entered, release) =
                crate::download::pipeline::real_410_abort_hook::install(movie_path.clone());
            let (abort, registration) = AbortHandle::new_pair();
            let selected_list = [&selected];
            let run_config = make_run_cycle_config();
            let cancel = CancellationToken::new();
            let mut future = Box::pin(Abortable::new(
                run_cycle(
                    &selected_list,
                    &run_config,
                    Some(db.as_ref()),
                    false,
                    &builder,
                    download::DownloadControls::download_hidden(),
                    &shared_session,
                    &cancel,
                ),
                registration,
            ));
            tokio::select! {
                ready = entered => ready.expect("real 410 hook owner must reach witness stage"),
                early = &mut future => panic!("cycle ended before real 410 witness: {early:?}"),
                () = tokio::time::sleep(std::time::Duration::from_secs(10)) => panic!("real 410 witness hook not reached"),
            }
            assert!(
                movie_gets.load(Ordering::SeqCst) > 0,
                "actual HTTP 410 must precede hook"
            );
            {
                let conn = db.acquire_lock("pre-commit synthetic 410").unwrap();
                let count: i64 = conn.query_row("SELECT COUNT(*) FROM provider_410_failure_epochs WHERE child='asset-shared'", [], |r| r.get(0)).unwrap();
                assert_eq!(count, 0, "hook must precede mandatory retry transaction");
            }
            abort.abort();
            assert!(future.as_mut().await.is_err(), "only the waiter is aborted");
            drop(future);
            assert!(
                !crate::download::pipeline::real_410_abort_hook::can_lock(
                    &movie_path,
                    std::time::Duration::from_millis(100)
                )
                .await,
                "destination guard must survive waiter abort"
            );
            release.send(()).unwrap();
            assert!(
                crate::download::pipeline::real_410_abort_hook::can_lock(
                    &movie_path,
                    std::time::Duration::from_secs(10)
                )
                .await,
                "guard releases only after 410 state sequence completes"
            );
            {
                let conn = db.acquire_lock("post-abort synthetic 410").unwrap();
                let (count, attempt, deadline): (i64, i64, i64) = conn.query_row("SELECT COUNT(*),COALESCE(MAX(attempt),0),COALESCE(MAX(deadline),0) FROM provider_410_failure_epochs WHERE child='asset-shared' AND version_size='live_original'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
                assert_eq!((count, attempt), (1, 1));
                assert!(deadline > crate::state::db::retry_clock::now_for_path(&database));
                let saved: i64 = conn.query_row("SELECT attempts FROM provider_active_decisions WHERE child='asset-shared' AND admission='admitted'", [], |r| r.get(0)).unwrap();
                assert_eq!(saved, 1, "real child retry debt survives waiter abort");
            }
            assert_eq!(
                db.get_metadata(&sync_token_key(SOURCE))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("current-0"),
                "aborted observation cannot advance its source cursor"
            );
            eprintln!(
                "ISSUE845_ABORTED_410_WAITER_REACHED actual_410=true guard_held=true epochs=1 attempts=1 cursor=current-0"
            );
            // This guard is process-local. Keep it installed until the detached
            // witness owner is drained and its durable retry evidence checked.
            drop(test_clock);
            return;
        }
        if !baseline
            && cycle == 1
            && (refresh_same_generation.is_some() || previously_dispatched_sibling)
        {
            events.lock().unwrap().push("reopen");
        }
        let result = if !baseline
            && cycle == 0
            && matches!(refresh_same_generation, Some(0 | 2 | 3))
        {
            let (entered, release) =
                crate::download::pipeline::real_410_abort_hook::install_refresh_ticket(
                    movie_path.clone(),
                );
            let selected_list = [&selected];
            let run_config = make_run_cycle_config();
            let cancel = CancellationToken::new();
            let mut future = Box::pin(run_cycle(
                &selected_list,
                &run_config,
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &cancel,
            ));
            tokio::select! {
                ready=entered=>ready.expect("actual selected refreshed ticket must reach guard"),
                early=&mut future=>panic!("selected cycle ended before refreshed ticket: {early:?}"),
                ()=tokio::time::sleep(std::time::Duration::from_secs(10))=>panic!("selected refreshed ticket was not admitted"),
            }
            assert_eq!(
                movie_gets.load(Ordering::SeqCst),
                1,
                "actual old 410 precedes selected refreshed ticket"
            );
            assert_eq!(
                fresh_gets.load(Ordering::SeqCst),
                0,
                "ticket is committed before fresh GET"
            );
            assert!(
                !crate::download::pipeline::real_410_abort_hook::can_lock(
                    &movie_path,
                    std::time::Duration::from_millis(100)
                )
                .await,
                "same destination guard remains held by the selected ticket"
            );
            {
                let conn = db
                    .acquire_lock("inflight authenticated selected refresh")
                    .unwrap();
                type RefreshedRow = (
                    String,
                    String,
                    String,
                    String,
                    String,
                    String,
                    String,
                    String,
                    Vec<u8>,
                    String,
                    String,
                    String,
                );
                let rows: Vec<RefreshedRow> = conn.prepare("SELECT t.id,t.proof_hash,t.generation,t.pass_key,t.child,t.version_size,t.path,t.phase,t.receipt,g.account_key,g.provider_key,g.scope FROM provider_refreshed_transfer t JOIN provider_active_generations g ON g.id=t.generation WHERE t.child='asset-shared' AND t.version_size='live_original' ORDER BY t.id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?))).unwrap().collect::<Result<_,_>>().unwrap();
                assert_eq!(rows.len(), 1, "one actual scoped in-flight ticket");
                let row = &rows[0];
                assert_eq!(row.1.len(), 64);
                assert_eq!(row.3, expected_pass);
                assert_eq!(row.4, "asset-shared");
                assert_eq!(row.5, "live_original");
                assert_eq!(row.7, "inflight");
                assert_eq!(
                    serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                        &row.6
                    )
                    .unwrap()
                    .to_path(),
                    movie_path
                );
                assert_eq!(row.9, account);
                assert_eq!(row.10, provider);
                assert_eq!(
                    serde_json::from_str::<Value>(&row.11).unwrap(),
                    json!({"container":"com.apple.photos.cloud","database":"shared","environment":"production","format":1,"realm":"com","zone":zone(SOURCE)})
                );
                let epoch:i64=conn.query_row("SELECT count(*) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child=?3 AND version_size=?4 AND path=?5 AND url=?6",rusqlite::params![row.2,row.3,row.4,row.5,row.6,format!("{}/expired-movie",server.uri())],|r|r.get(0)).unwrap();
                assert_eq!(
                    epoch, 1,
                    "ticket binds exact original selected epoch and destination"
                );
                let retained: Value = serde_json::from_slice(&row.8).unwrap();
                let request: Vec<u8> = serde_json::from_value(retained["request"].clone()).unwrap();
                let response: Vec<u8> =
                    serde_json::from_value(retained["response"].clone()).unwrap();
                let request: Value = serde_json::from_slice(&request).unwrap();
                let response: Value = serde_json::from_slice(&response).unwrap();
                assert_eq!(request["zoneID"], zone(SOURCE));
                let names = request["records"].as_array().unwrap();
                assert_eq!(names.len(), 2);
                assert!(names.iter().any(|r| r["recordName"] == "asset-shared"));
                assert!(names.iter().any(|r| r["recordName"] == "shared"));
                let master = response["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["recordName"] == "shared")
                    .unwrap();
                assert_eq!(master["zoneID"], zone(SOURCE));
                assert_eq!(master["recordChangeTag"], "ct-master");
                assert_eq!(
                    master["fields"]["resOriginalVidComplRes"]["value"]["downloadURL"],
                    format!("{}/fresh-movie", server.uri())
                );
                if refresh_same_generation == Some(3) {
                    non410_before_attempts=Some(conn.query_row("SELECT download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap());
                }
                ticket_evidence = Some((row.0.clone(), row.1.clone(), row.2.clone()));
            }
            release.send(()).unwrap();
            future.await.unwrap()
        } else {
            run_cycle(
                &[&selected],
                &make_run_cycle_config(),
                Some(db.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared_session,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
        };
        if baseline {
            assert_eq!(result.stats.downloaded, 0, "empty baseline has no media");
            assert_eq!(still_gets.load(Ordering::SeqCst), 0);
            assert_eq!(movie_gets.load(Ordering::SeqCst), 0);
            assert!(std::fs::read_dir(&root).unwrap().next().is_none());
            assert_eq!(
                db.get_metadata(&sync_token_key(SOURCE))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("current-0")
            );
            let requests = provider_requests.lock().unwrap();
            assert!(
                requests
                    .iter()
                    .any(|(url, body)| url.contains("/changes/zone?")
                        && body["zones"][0]["zoneID"] == zone(SOURCE)
                        && body["zones"][0]["syncToken"] == "boundary")
            );
            if approved_wait || refresh_same_generation.is_some() || previously_dispatched_sibling {
                test_clock = Some(crate::state::db::retry_clock::test_clock::Control::install(
                    &database,
                    chrono::Utc::now().timestamp(),
                ));
            }
            eprintln!("ISSUE845_EMPTY_BASELINE_REACHED cursor=current-0 media=0");
            continue;
        }
        if refresh_same_generation == Some(1) {
            assert_eq!(cycle, 0);
            assert_eq!(
                movie_gets.load(Ordering::SeqCst),
                1,
                "initial movie 410 reached"
            );
            assert_eq!(
                fresh_gets.load(Ordering::SeqCst),
                0,
                "no refreshed GET before durable state authorization"
            );
            assert_eq!(still_gets.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read(&first_path).unwrap(), occupant);
            assert_eq!(std::fs::read(&still_path).unwrap(), still);
            assert!(!movie_path.exists());
            {
                let conn = db.acquire_lock("selected refresh held").unwrap();
                let epochs: i64 = conn.query_row("SELECT COUNT(*) FROM provider_410_failure_epochs WHERE child='asset-shared' AND version_size='live_original'", [], |r| r.get(0)).unwrap();
                let receipts: i64 = conn.query_row("SELECT COUNT(*) FROM download_receipts WHERE child='asset-shared' AND version_size='live_original'", [], |r| r.get(0)).unwrap();
                let shared_waits: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM provider_410_shared_waits WHERE child='asset-shared'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let attempts: i64 = conn.query_row("SELECT attempts FROM provider_active_decisions WHERE child='asset-shared' AND admission='admitted'", [], |r| r.get(0)).unwrap();
                assert_eq!((epochs, receipts, shared_waits, attempts), (1, 0, 0, 1));
                assert_eq!(
                    result.stats.failed, 1,
                    "unresolved selected work is reported pending rather than as a shared wait"
                );
            }
            assert_eq!(
                db.get_metadata(&sync_token_key(SOURCE))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("current-0")
            );
            eprintln!(
                "ISSUE845_SELECTED_REFRESH_PENDING_REACHED old_410=1 fresh_get=0 movie_receipt=0 cursor=current-0"
            );
            return;
        }
        if refresh_same_generation == Some(3) {
            assert_eq!(cycle, 0);
            assert_eq!(movie_gets.load(Ordering::SeqCst), 1);
            assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
            assert_eq!(still_gets.load(Ordering::SeqCst), 1);
            assert_eq!(independent_gets.load(Ordering::SeqCst), 1);
            assert_eq!(std::fs::read(&first_path).unwrap(), occupant);
            assert_eq!(std::fs::read(&still_path).unwrap(), still);
            assert_eq!(
                std::fs::read(root.join(&date).join("independent.JPG")).unwrap(),
                MEDIA
            );
            assert!(!movie_path.exists());
            let conn = db.acquire_lock("actual selected refreshed 503").unwrap();
            let (ticket, proof, generation) = ticket_evidence.as_ref().unwrap();
            let terminal:String=conn.query_row("SELECT json_array(id,proof_hash,generation,phase,failure_status,failure_error_hash,outcome_proof) FROM provider_refreshed_transfer WHERE id=?1",[ticket],|r|r.get(0)).unwrap();
            let value: Value = serde_json::from_str(&terminal).unwrap();
            assert_eq!(value[0], *ticket);
            assert_eq!(value[1], *proof);
            assert_eq!(value[2], *generation);
            assert_eq!(value[3], "failed");
            assert_eq!(value[4], 503);
            assert_eq!(value[5].as_str().unwrap().len(), 64);
            assert_eq!(value[6].as_str().unwrap().len(), 64);
            let in_flight:i64=conn.query_row("SELECT count(*) FROM provider_refreshed_transfer WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original' AND phase='inflight'",rusqlite::params![generation,expected_pass],|r|r.get(0)).unwrap();
            assert_eq!(in_flight, 0);
            let added_410:i64=conn.query_row("SELECT count(*) FROM provider_refreshed_transfer WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original' AND phase='expired'",rusqlite::params![generation,expected_pass],|r|r.get(0)).unwrap();
            assert_eq!(added_410, 0);
            let original_410:i64=conn.query_row("SELECT count(*) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original'",rusqlite::params![generation,expected_pass],|r|r.get(0)).unwrap();
            assert_eq!(original_410, 1, "retain actual old 410 only");
            let (status,attempts):(String,i64)=conn.query_row("SELECT status,download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='live_original'",[SOURCE],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(status, "failed");
            assert_eq!(
                attempts,
                non410_before_attempts.unwrap() + 1,
                "transaction charges actual 503 exactly once; generic finalizer must not double-charge"
            );
            let movie_receipts:i64=conn.query_row("SELECT count(*) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap();
            assert_eq!(movie_receipts, 0);
            let independent_receipts:i64=conn.query_row("SELECT count(*) FROM download_receipts WHERE library=?1 AND child='asset-independent' AND version_size='original'",[SOURCE],|r|r.get(0)).unwrap();
            assert_eq!(
                independent_receipts, 1,
                "independent selected work progresses"
            );
            let still_receipt:String=conn.query_row("SELECT json_array(account_key,provider_key,scope,library,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,proof_hash) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='original'",[SOURCE],|r|r.get(0)).unwrap();
            non410_still = Some((
                still_receipt,
                std::fs::metadata(&still_path).unwrap().modified().unwrap(),
            ));
            non410_terminal = Some(terminal);
            assert!(
                result.stats.failed > 0,
                "actual 503 remains reported unsuccessful"
            );
            drop(conn);
            continue;
        }
        if matches!(refresh_same_generation, Some(0 | 2)) {
            assert_eq!(std::fs::read(&first_path).unwrap(), occupant);
            assert_eq!(std::fs::read(&still_path).unwrap(), still);
            assert_eq!(still_gets.load(Ordering::SeqCst), 1);
            let conn = db
                .acquire_lock("selected refresh terminal evidence")
                .unwrap();
            let still_receipt:String=conn.query_row("SELECT json_array(account_key,provider_key,scope,library,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,proof_hash) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='original'",[SOURCE],|r|r.get(0)).unwrap();
            let stamp = std::fs::metadata(&still_path).unwrap().modified().unwrap();
            if let Some((prior, old_stamp)) = &retained_still {
                assert_eq!(&still_receipt, prior);
                assert_eq!(stamp, *old_stamp);
            } else {
                retained_still = Some((still_receipt, stamp));
            }
            let (ticket, proof, generation) = ticket_evidence
                .as_ref()
                .expect("real in-flight ticket captured under guard");
            let row:String=conn.query_row("SELECT json_array(id,proof_hash,generation,phase,attempt,deadline,outcome_proof) FROM provider_refreshed_transfer WHERE id=?1",[ticket],|r|r.get(0)).unwrap();
            let parsed: Value = serde_json::from_str(&row).unwrap();
            assert_eq!(parsed[0], *ticket);
            assert_eq!(parsed[1], *proof);
            assert_eq!(parsed[2], *generation);
            if refresh_same_generation == Some(2) {
                assert_eq!(parsed[3], "expired");
                assert!(parsed[4].as_i64().unwrap() > 0);
                assert!(
                    parsed[5].as_i64().unwrap()
                        > crate::state::db::retry_clock::now_for_path(&database)
                );
                assert_eq!(parsed[6].as_str().unwrap().len(), 64);
                assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
                let movie_receipts:i64=conn.query_row("SELECT count(*) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap();
                assert_eq!(movie_receipts, 0);
                assert!(!movie_path.exists());
                let old_epoch:i64=conn.query_row("SELECT count(*) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original' AND url=?3",rusqlite::params![generation,expected_pass,format!("{}/expired-movie",server.uri())],|r|r.get(0)).unwrap();
                assert_eq!(old_epoch, 1);
                ticket_outcome = Some(row);
                assert!(result.stats.failed > 0, "refreshed 410 remains unfinished");
                drop(conn);
                continue;
            }
            assert_eq!(parsed[3], "published");
            assert!(parsed[4].is_null());
            assert!(parsed[5].is_null());
            assert_eq!(parsed[6].as_str().unwrap().len(), 64);
            let archived:(String,String) = conn.query_row("SELECT receipt_id,proof_hash FROM provider_refreshed_success_receipts WHERE dispatch_id=?1",[ticket],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(archived.0.len(), 64);
            assert_eq!(archived.1.len(), 64);
            let receipt_id: String = conn
                .query_row(
                    "SELECT success_receipt_id FROM provider_refreshed_transfer WHERE id=?1",
                    [ticket],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(receipt_id, archived.0);
            if let Some(prior) = &ticket_outcome {
                assert_eq!(&row, prior);
            } else {
                ticket_outcome = Some(row);
            }
            drop(conn);
        }
        if refresh_same_generation.is_some() || previously_dispatched_sibling {
            assert_eq!(std::fs::read(&first_path).unwrap(), occupant);
            let request_log = provider_requests.lock().unwrap().clone();
            if cycle == 0 {
                assert!(
                    request_log
                        .iter()
                        .any(|(url, body)| url.contains("/changes/zone?")
                            && body["zones"][0]["zoneID"] == zone(SOURCE)
                            && body["zones"][0]["syncToken"] == "current-0")
                );
            }
            {
                let conn = db.acquire_lock("synthetic generation controls").unwrap();
                let epochs: Vec<String> = conn.prepare("SELECT json_array(e.generation,e.pass_key,e.child,e.version_size,e.path,e.attempt,e.deadline,e.marker_hash,e.url,g.account_key,g.provider_key,g.scope) FROM provider_410_failure_epochs e JOIN provider_active_generations g ON g.id=e.generation WHERE e.child='asset-shared' AND e.version_size='live_original' ORDER BY e.attempt,e.deadline")
                .unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
                assert_eq!(epochs.len(), 1, "preserve the actual first 410 epoch");
                let epoch: Value = serde_json::from_str(&epochs[0]).unwrap();
                assert_eq!(epoch[1], expected_pass);
                assert_eq!(epoch[2], "asset-shared");
                assert_eq!(epoch[3], "live_original");
                let native: crate::state::db::provider_selection::SelectionPath =
                    serde_json::from_str(epoch[4].as_str().unwrap()).unwrap();
                assert_eq!(native.to_path(), movie_path);
                assert_eq!(epoch[5], 1);
                assert!(
                    epoch[6].as_i64().unwrap()
                        > crate::state::db::retry_clock::now_for_path(&database)
                );
                assert_eq!(epoch[7].as_str().unwrap().len(), 64);
                assert_eq!(epoch[8], format!("{}/expired-movie", server.uri()));
                assert_eq!(epoch[9], account);
                assert_eq!(epoch[10], provider);
                assert_eq!(
                    serde_json::from_str::<Value>(epoch[11].as_str().unwrap()).unwrap(),
                    json!({"container":"com.apple.photos.cloud","database":"shared","environment":"production","format":1,"realm":"com","zone":zone(SOURCE)})
                );
                if let Some(previous) = &saved_component_epoch {
                    assert_eq!(&epochs[0], previous);
                }
                saved_component_epoch = Some(epochs[0].clone());
                let (admission, attempts, deadline, retry_policy, admitted_policy): (String,i64,i64,String,String) = conn.query_row(
                "SELECT d.admission,d.attempts,d.next_retry_at,json_extract(CAST(g.specification AS TEXT),'$.profile.retry_policy'),json_extract(CAST(g.specification AS TEXT),'$.profile.admitted_retry_policy') FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE d.generation=?1 AND d.pass_key=?2 AND d.child=?3",
                rusqlite::params![epoch[0].as_str().unwrap(),expected_pass,"asset-shared"], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
                assert_eq!(admission, "admitted");
                assert_eq!(attempts, epoch[5].as_i64().unwrap());
                assert_eq!(
                    deadline,
                    epoch[6].as_i64().unwrap(),
                    "child deadline is exactly the retained movie 410 epoch"
                );
                assert_eq!(retry_policy.len(), 64);
                assert_eq!(admitted_policy.len(), 64);
                if refresh_same_generation.is_some() {
                    assert_eq!(
                        movie_gets.load(Ordering::SeqCst),
                        1,
                        "only the original URL gets a 410"
                    );
                    assert_eq!(
                        fresh_gets.load(Ordering::SeqCst),
                        1,
                        "same-generation fresh URL must be independently evaluated"
                    );
                    if cycle == 0 {
                        assert!(
                            request_log
                                .iter()
                                .any(|(url, body)| url.contains("/records/lookup?")
                                    && body["records"].as_array().is_some_and(|items| items
                                        .iter()
                                        .any(|r| r["recordName"] == "asset-shared"))),
                            "real scoped refresh lookup"
                        );
                    }
                    assert_eq!(std::fs::read(&movie_path).unwrap(), movie);
                    let stamp = std::fs::metadata(&movie_path).unwrap().modified().unwrap();
                    if let Some(previous) = completed_movie_stamp {
                        assert_eq!(stamp, previous);
                    }
                    completed_movie_stamp = Some(stamp);
                    let receipt: (String,String,i64,String) = conn.query_row("SELECT origin_pass,provider_checksum,provider_size,native_path FROM download_receipts WHERE child='asset-shared' AND version_size='live_original'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
                    assert_eq!(receipt.0, expected_pass);
                    let scoped_count: i64 = conn.query_row("SELECT count(*) FROM download_receipts r JOIN provider_active_destinations w ON w.generation=?1 AND w.pass_key=r.origin_pass AND w.child=r.child AND w.master=r.master AND w.version_size=r.version_size AND w.path=r.native_path AND w.checksum=r.provider_checksum AND w.size_bytes=r.provider_size WHERE r.account_key=?2 AND r.provider_key=?3 AND r.scope=?4 AND r.library=?5 AND r.child='asset-shared' AND r.version_size='live_original'", rusqlite::params![epoch[0].as_str().unwrap(),account,provider,epoch[11].as_str().unwrap(),SOURCE], |r| r.get(0)).unwrap();
                    assert_eq!(
                        scoped_count, 1,
                        "fresh receipt must bind the original epoch generation and authenticated destination"
                    );
                    let events_now = events.lock().unwrap().clone();
                    let failed = events_now.iter().position(|e| *e == "movie:410").unwrap();
                    let child = events_now
                        .iter()
                        .position(|e| *e == "lookup:child")
                        .unwrap();
                    let pair = events_now.iter().position(|e| *e == "lookup:pair").unwrap();
                    let success = events_now.iter().position(|e| *e == "movie:200").unwrap();
                    assert!(
                        failed < child && child < pair && pair < success,
                        "410, scoped child/master lookup and fresh GET ordering: {events_now:?}"
                    );
                    assert_eq!(
                        receipt.1,
                        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
                    );
                    assert_eq!(receipt.2, i64::try_from(movie.len()).unwrap());
                    assert_eq!(
                    serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                        &receipt.3
                    )
                    .unwrap()
                    .to_path(),
                    movie_path
                );
                    assert_eq!(std::fs::read(&still_path).unwrap(), still);
                    assert_eq!(still_gets.load(Ordering::SeqCst), 1);
                } else if cycle == 0 {
                    let still_count = still_gets.load(Ordering::SeqCst);
                    let movie_count = movie_gets.load(Ordering::SeqCst);
                    assert!(
                        (1..=2).contains(&still_count),
                        "bounded actual still 503 requests: {still_count}"
                    );
                    assert!(
                        (1..=2).contains(&movie_count),
                        "bounded actual movie 410 requests: {movie_count}"
                    );
                    let events_now = events.lock().unwrap().clone();
                    let still_first = events_now.iter().position(|e| *e == "still:503").unwrap();
                    let movie_first = events_now.iter().position(|e| *e == "movie:410").unwrap();
                    assert!(
                        still_first < movie_first,
                        "real still 503 precedes movie 410: {events_now:?}"
                    );
                    assert_eq!(
                        events_now.iter().filter(|e| **e == "still:503").count(),
                        still_count
                    );
                    assert_eq!(
                        events_now.iter().filter(|e| **e == "movie:410").count(),
                        movie_count
                    );
                    assert!(!events_now.contains(&"still:200"));
                    let (status, asset_attempts): (String,i64) = conn.query_row("SELECT status,download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='original'", [SOURCE], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
                    assert_eq!(status, "failed");
                    assert!(
                        asset_attempts >= 1
                            && asset_attempts <= i64::try_from(still_count).unwrap(),
                        "still has its own ordinary failure record; no 410 epoch"
                    );
                    assert!(!still_path.exists());
                    let dispatch: Vec<(String,String)> = conn.prepare("SELECT version_size,state FROM provider_transfer_dispatch WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' ORDER BY version_size")
                    .unwrap().query_map(rusqlite::params![epoch[0].as_str().unwrap(),expected_pass], |r| Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
                    assert_eq!(
                        dispatch,
                        vec![
                            ("live_original".into(), "dispatched".into()),
                            ("original".into(), "dispatched".into())
                        ]
                    );
                    let proofs: i64 = conn.query_row("SELECT COUNT(*) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='original'",rusqlite::params![epoch[0].as_str().unwrap(),expected_pass], |r| r.get(0)).unwrap();
                    assert_eq!(proofs, 0, "503 sibling must not invent a 410 marker");
                    still_ready.store(true, Ordering::SeqCst);
                } else {
                    assert_eq!(
                        movie_gets.load(Ordering::SeqCst) - before_requests,
                        0,
                        "movie 410 wait remains future"
                    );
                    assert_eq!(
                        still_gets.load(Ordering::SeqCst) - before_still_requests,
                        1,
                        "previously dispatched independent sibling must progress"
                    );
                    let events_now = events.lock().unwrap().clone();
                    let still_first = events_now.iter().position(|e| *e == "still:503").unwrap();
                    let movie_first = events_now.iter().position(|e| *e == "movie:410").unwrap();
                    let reopen = events_now.iter().position(|e| *e == "reopen").unwrap();
                    let success = events_now.iter().position(|e| *e == "still:200").unwrap();
                    assert!(
                        still_first < movie_first && movie_first < reopen && reopen < success,
                        "503 -> 410 -> reopen -> 200: {events_now:?}"
                    );
                    assert_eq!(events_now.iter().filter(|e| **e == "still:200").count(), 1);
                    assert_eq!(
                        events_now[reopen + 1..]
                            .iter()
                            .filter(|e| **e == "movie:410")
                            .count(),
                        0
                    );
                    assert_eq!(std::fs::read(&still_path).unwrap(), still);
                    let receipt: (String,String) = conn.query_row("SELECT origin_pass,native_path FROM download_receipts WHERE child='asset-shared' AND version_size='original'",[],|r| Ok((r.get(0)?,r.get(1)?))).unwrap();
                    assert_eq!(receipt.0, expected_pass);
                    let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts r JOIN provider_active_destinations w ON w.generation=?1 AND w.pass_key=r.origin_pass AND w.child=r.child AND w.master=r.master AND w.version_size=r.version_size AND w.path=r.native_path AND w.checksum=r.provider_checksum AND w.size_bytes=r.provider_size WHERE r.account_key=?2 AND r.provider_key=?3 AND r.scope=?4 AND r.library=?5 AND r.child='asset-shared' AND r.version_size='original'", rusqlite::params![epoch[0].as_str().unwrap(),account,provider,epoch[11].as_str().unwrap(),SOURCE], |r| r.get(0)).unwrap();
                    assert_eq!(
                        count, 1,
                        "receipt remains under the same authenticated generation and native destination"
                    );
                    assert_eq!(
                    serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                        &receipt.1
                    )
                    .unwrap()
                    .to_path(),
                    still_path
                );
                }
            }
            if cycle == 1 {
                assert_eq!(
                    db.get_metadata(&sync_token_key(SOURCE))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("current-2")
                );
                eprintln!(
                    "ISSUE845_GENERATION_CONTROL_REACHED refreshed={:?} dispatched_sibling={previously_dispatched_sibling}",
                    refresh_same_generation.is_some()
                );
                if refresh_same_generation == Some(0) {
                    eprintln!(
                        "ISSUE845_SELECTED_REFRESH_TICKET_SUCCESS_REACHED ticket=published guard=held scoped_receipt=1 stable_reopen=true"
                    );
                }
                return;
            }
            continue;
        }
        assert_eq!(
            std::fs::read(&first_path).unwrap(),
            occupant,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            std::fs::read(&still_path).unwrap(),
            still,
            "cycle {cycle}: {result:?}"
        );
        fn collect_tree(
            path: &std::path::Path,
            found: &mut std::collections::BTreeSet<std::path::PathBuf>,
        ) {
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                assert!(!kind.is_symlink(), "unexpected symlink");
                assert!(
                    kind.is_dir() || kind.is_file(),
                    "unexpected filesystem entry"
                );
                let path = entry.path();
                assert!(found.insert(path.clone()));
                if kind.is_dir() {
                    collect_tree(&path, found);
                }
            }
        }
        let mut actual_tree = std::collections::BTreeSet::new();
        collect_tree(&root, &mut actual_tree);
        let mut expected_tree =
            std::collections::BTreeSet::from([first_path.clone(), still_path.clone()]);
        if approved_wait && repeated_observation && cycle >= 1 {
            let independent = root.join(&date).join("independent.JPG");
            assert_eq!(std::fs::read(&independent).unwrap(), MEDIA);
            let stamp = std::fs::metadata(&independent).unwrap().modified().unwrap();
            if let Some(previous) = independent_stamp {
                assert_eq!(stamp, previous);
            }
            independent_stamp = Some(stamp);
            expected_tree.insert(independent);
            assert_eq!(
                independent_gets.load(Ordering::SeqCst),
                1,
                "independent work progresses exactly once"
            );
        } else {
            assert_eq!(independent_gets.load(Ordering::SeqCst), 0);
        }
        let mut parent = first_path.parent().unwrap();
        while parent != root.as_path() {
            expected_tree.insert(parent.to_path_buf());
            parent = parent.parent().unwrap();
        }
        assert_eq!(
            actual_tree, expected_tree,
            "complete media tree on cycle {cycle}"
        );
        let first_stamp = std::fs::metadata(&first_path).unwrap().modified().unwrap();
        if let Some(previous) = occupant_stamp {
            assert_eq!(first_stamp, previous);
        }
        occupant_stamp = Some(first_stamp);
        let stamp = std::fs::metadata(&still_path).unwrap().modified().unwrap();
        if let Some(previous) = saved_stamp {
            assert_eq!(stamp, previous);
        }
        saved_stamp = Some(stamp);
        let still_requests = still_gets.load(Ordering::SeqCst) - before_still_requests;
        assert_eq!(
            still_requests,
            usize::from(cycle == 0),
            "per-cycle still GETs"
        );
        assert_eq!(still_gets.load(Ordering::SeqCst), 1);
        assert!(!movie_path.exists());
        let conn = db.acquire_lock("issue845 scoped receipts").unwrap();
        let still_receipts: i64 = conn.query_row(
            "SELECT count(*) FROM download_receipts WHERE child='asset-shared' AND version_size='original'",
            [], |row| row.get(0)).unwrap();
        assert_eq!(still_receipts, 1, "exact scoped still receipt count");
        let receipt: String = conn.query_row(
            "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='original'",
            [SOURCE], |row| row.get(0)).unwrap();
        let value: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(value[0], account);
        assert_eq!(value[1], provider);
        let scope: Value = serde_json::from_str(value[2].as_str().unwrap()).unwrap();
        assert_eq!(
            scope,
            json!({
                "container":"com.apple.photos.cloud","database":"shared",
                "environment":"production","format":1,"realm":"com","zone":zone(SOURCE)
            })
        );
        assert_eq!(value[3], SOURCE);
        assert_eq!(value[4], "asset-shared");
        assert_eq!(value[5], "shared");
        assert_eq!(value[6], "asset-shared");
        assert_eq!(value[7], expected_pass);
        assert_eq!(value[8], "original");
        assert_eq!(
            value[9],
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still))
        );
        assert_eq!(value[10], still.len());
        let native: crate::state::db::provider_selection::SelectionPath =
            serde_json::from_str(value[11].as_str().unwrap()).unwrap();
        assert_eq!(native.to_path(), still_path);
        let hash = format!("{:x}", Sha256::digest(still));
        assert_eq!(value[12], hash);
        assert_eq!(value[13], hash);
        assert_eq!(value[14].as_str().unwrap().len(), 64);
        if let Some(previous) = &saved_receipt {
            assert_eq!(&receipt, previous);
        }
        saved_receipt = Some(receipt);
        let movie_receipts: i64 = conn.query_row(
            "SELECT count(*) FROM download_receipts WHERE child='asset-shared' AND version_size='live_original'",
            [], |row| row.get(0)).unwrap();
        assert_eq!(movie_receipts, 0);
        let pending: Vec<(String, String, String, String, String, i64, i64)> = conn.prepare(
            "SELECT library,master,version_size,path,checksum,size_bytes,admitted FROM provider_active_destinations WHERE child='asset-shared' AND verified_media=0 ORDER BY library,master,version_size,path,checksum,size_bytes,admitted"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))
            .unwrap().collect::<Result<_,_>>().unwrap();
        let actual_destinations: std::collections::BTreeSet<_> = pending
            .iter()
            .map(
                |(library, master, version, path, checksum, size, admitted)| {
                    (
                    library.clone(),
                    master.clone(),
                    version.clone(),
                    serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                        path,
                    )
                    .unwrap()
                    .to_path(),
                    checksum.clone(),
                    *size,
                    *admitted,
                )
                },
            )
            .collect();
        let expected_destinations = std::collections::BTreeSet::from([(
            SOURCE.to_owned(),
            "shared".to_owned(),
            "live_original".to_owned(),
            movie_path.clone(),
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie)),
            movie.len() as i64,
            1,
        )]);
        if changed_resource && cycle == 1 {
            assert!(
                expected_destinations.is_subset(&actual_destinations),
                "old resource remains pending"
            );
            let newer: Vec<_> = actual_destinations
                .difference(&expected_destinations)
                .collect();
            assert_eq!(
                newer.len(),
                1,
                "exactly one independently admitted new resource"
            );
            assert_eq!(newer[0].0, SOURCE);
            assert_eq!(newer[0].1, "shared");
            assert_eq!(newer[0].2, "live_original");
            assert!(newer[0].3.starts_with(&root));
            assert_ne!(
                newer[0].3, movie_path,
                "different resources keep distinct destinations"
            );
            assert_eq!(
                newer[0].4,
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&changed_movie))
            );
            assert_eq!(newer[0].5, changed_movie.len() as i64);
            assert_eq!(newer[0].6, 1);
        } else {
            assert_eq!(
                actual_destinations, expected_destinations,
                "exact pending destinations on cycle {cycle}"
            );
        }
        let retry_rows: Vec<String> = conn.prepare(
            "SELECT json_array(g.id,g.account_key,g.provider_key,g.scope,d.pass_key,d.child,d.admission,d.reason,d.attempts,d.next_retry_at) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-shared' AND EXISTS (SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND w.verified_media=0) ORDER BY g.id,d.pass_key"
        ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        // Diagnostic only: preserve the count assertion below. These rows and
        // provider requests contain synthetic identities and localhost URLs.
        let diagnostic_rows: Vec<String> = conn.prepare(
            "SELECT json_array(g.id,g.account_key,g.provider_key,g.scope,g.config_hash,g.basis,g.metadata_enabled,CAST(g.specification AS TEXT),g.specification_hash,g.sealed,g.checkpoint_ready,g.checkpoint_veto,g.seal_hash,g.seal_header_hash,g.created_at,g.replay_after,g.last_replayed_at,g.charged_bytes,d.pass_key,d.child,CAST(d.manifest AS TEXT),d.manifest_hash,d.outcome,d.admission,d.reason,d.admission_hash,d.attempts,d.next_retry_at,d.charged_bytes,d.compacted_hash,w.library,w.asset_id,w.master,w.version_size,w.path,w.compat_path,w.checksum,w.size_bytes,w.created_at,w.added_at,w.metadata_hash,w.admitted,w.verified_media,w.verified_metadata,w.local_checksum,w.source_checksum,w.grouping_hash,w.intent_hash,w.progress_hash,w.prepared_checksum,w.prepared_size,w.prepared_hash) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation LEFT JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child WHERE d.child='asset-shared' ORDER BY g.id,d.pass_key,w.version_size,w.path"
        ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        let request_trace: Vec<Value> = {
            let requests = provider_requests.lock().unwrap();
            requests.iter().map(|(url, body)| json!({
                "endpoint": if url.contains("/changes/zone?") { "changes" }
                    else if url.contains("/records/lookup?") { "lookup" }
                    else if url.contains("/records/query?") { "query" }
                    else if url.contains("/zones/list") { "zones" }
                    else { "other" },
                "zone": body["zoneID"]["zoneName"].as_str()
                    .or_else(|| body["zones"][0]["zoneID"]["zoneName"].as_str()),
                "cursor": body["zones"][0]["syncToken"],
                "record_names": body["records"].as_array().map(|items|
                    items.iter().map(|item| item["recordName"].clone()).collect::<Vec<_>>()),
                "query_type": body["query"]["recordType"]
            })).collect()
        };
        if cycle == 1 {
            let changes = request_trace
                .iter()
                .filter(|request| {
                    request["endpoint"] == "changes"
                        && request["zone"] == SOURCE
                        && request["cursor"]
                            == if approved_wait && repeated_observation {
                                "current-0"
                            } else {
                                "current-1"
                            }
                })
                .count();
            assert_eq!(
                changes, 1,
                "the intended Shared changes page must be requested"
            );
        }
        eprintln!(
            "ISSUE845_DIAG_COLUMNS owner_destinations=[generation,account,provider,scope,config_hash,basis,metadata_enabled,specification,specification_hash,sealed,checkpoint_ready,checkpoint_veto,seal_hash,seal_header_hash,created_at,replay_after,last_replayed_at,generation_charged_bytes,pass,child,manifest,manifest_hash,outcome,admission,reason,admission_hash,attempts,next_retry_at,decision_charged_bytes,compacted_hash,library,asset_id,master,version,path,compat_path,checksum,size,created_at,added_at,metadata_hash,admitted,verified_media,verified_metadata,local_checksum,source_checksum,grouping_hash,intent_hash,progress_hash,prepared_checksum,prepared_size,prepared_hash]"
        );
        eprintln!(
            "ISSUE845_DIAG cycle={cycle} previous_retry={saved_retry:?} still_gets={} movie_gets={} retry_rows={retry_rows:?} owner_destinations={diagnostic_rows:?} provider_requests={request_trace:?}",
            still_gets.load(Ordering::SeqCst),
            movie_gets.load(Ordering::SeqCst)
        );
        // Compare immutable source identity, resource manifest (including the
        // synthetic URLs), destination and retry policy without equating byte
        // matches to cross-account ownership or requiring a particular count.
        let equivalent_rows: Vec<String> = conn.prepare(
            "SELECT json_array(g.id,g.account_key,g.provider_key,g.scope,g.config_hash,json_extract(CAST(g.specification AS TEXT),'$.profile.retry_policy'),json_extract(CAST(g.specification AS TEXT),'$.profile.admitted_retry_policy'),d.pass_key,d.child,d.manifest_hash,CAST(d.manifest AS TEXT),d.attempts,d.next_retry_at) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-shared' AND EXISTS (SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND w.verified_media=0) ORDER BY g.id,d.pass_key"
        ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        if cycle == 0 {
            assert_eq!(equivalent_rows.len(), 1, "initial owned obligation");
            original_equivalence = Some(serde_json::from_str(&equivalent_rows[0]).unwrap());
        }
        if changed_resource && cycle == 1 {
            let prior = saved_retry.as_ref().expect("cycle-zero retry");
            assert!(
                crate::state::db::retry_clock::now_for_path(&database) < prior[9].as_i64().unwrap()
            );
            assert!(
                retry_rows
                    .iter()
                    .any(|encoded| serde_json::from_str::<Value>(encoded).unwrap() == *prior),
                "older changed-resource obligation remains independently owned"
            );
            let original = original_equivalence.as_ref().unwrap();
            let mut changed_confirmed = false;
            for encoded in &equivalent_rows {
                let row: Value = serde_json::from_str(encoded).unwrap();
                if row[0] == original[0] {
                    continue;
                }
                for index in 1..=8 {
                    assert_eq!(row[index], original[index], "exact scope and retry policy");
                }
                let manifest: Value = serde_json::from_str(row[10].as_str().unwrap()).unwrap();
                let confirmation: Vec<u8> =
                    serde_json::from_value(manifest["decision"]["confirmation"].clone()).unwrap();
                let source: Value = serde_json::from_slice(&confirmation).unwrap();
                let master = source["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| record["recordName"] == "shared")
                    .unwrap();
                if master["fields"]["resOriginalVidComplRes"]["value"]["downloadURL"]
                    == format!("{}/changed-movie", server.uri())
                {
                    assert_eq!(
                        master["fields"]["resOriginalVidComplRes"]["value"]["fileChecksum"],
                        base64::engine::general_purpose::STANDARD
                            .encode(Sha256::digest(&changed_movie))
                    );
                    assert_eq!(
                        master["fields"]["resOriginalVidComplRes"]["value"]["size"],
                        changed_movie.len()
                    );
                    changed_confirmed = true;
                }
            }
            assert!(
                changed_confirmed,
                "new resource retains its own source proof"
            );
            assert_eq!(movie_gets.load(Ordering::SeqCst) - before_requests, 0);
            let changed = changed_gets.load(Ordering::SeqCst);
            assert!(
                (1..=2).contains(&changed),
                "new resource is independently evaluated"
            );
            eprintln!(
                "ISSUE845_CHANGED_RESOURCE_REACHED old_retry_future=true old_url_gets=0 changed_gets={changed}"
            );
            drop(conn);
            continue;
        }
        if changed_url && cycle == 1 {
            let prior = saved_retry.as_ref().expect("cycle-zero retry");
            assert!(
                crate::state::db::retry_clock::now_for_path(&database) < prior[9].as_i64().unwrap(),
                "changed URL is evaluated while original wait remains future"
            );
            assert!(
                retry_rows
                    .iter()
                    .any(|encoded| serde_json::from_str::<Value>(encoded).unwrap() == *prior),
                "old retry owner and deadline remain intact"
            );
            let original = original_equivalence.as_ref().unwrap();
            let mut changed_confirmed = false;
            for encoded in &equivalent_rows {
                let row: Value = serde_json::from_str(encoded).unwrap();
                if row[0] == original[0] {
                    continue;
                }
                for index in 1..=8 {
                    assert_eq!(row[index], original[index], "exact source and retry scope");
                }
                let manifest: Value = serde_json::from_str(row[10].as_str().unwrap()).unwrap();
                let confirmation: Vec<u8> =
                    serde_json::from_value(manifest["decision"]["confirmation"].clone()).unwrap();
                let source: Value = serde_json::from_slice(&confirmation).unwrap();
                let master = source["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| record["recordName"] == "shared")
                    .unwrap();
                if master["fields"]["resOriginalVidComplRes"]["value"]["downloadURL"]
                    == format!("{}/fresh-movie", server.uri())
                {
                    assert_eq!(
                        master["fields"]["resOriginalVidComplRes"]["value"]["fileChecksum"],
                        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
                    );
                    assert_eq!(
                        master["fields"]["resOriginalVidComplRes"]["value"]["size"],
                        movie.len()
                    );
                    assert_eq!(master["recordChangeTag"], "ct-master");
                    changed_confirmed = true;
                }
            }
            assert!(
                changed_confirmed,
                "new owner retains exact refreshed URL evidence"
            );
            assert_eq!(
                movie_gets.load(Ordering::SeqCst) - before_requests,
                0,
                "old expired URL must not substitute for current URL"
            );
            let fresh = fresh_gets.load(Ordering::SeqCst);
            assert!(
                (1..=2).contains(&fresh),
                "changed URL must be independently evaluated with bounded GETs"
            );
            eprintln!(
                "ISSUE845_CHANGED_URL_REACHED old_retry_future=true old_url_gets=0 fresh_url_gets={fresh}"
            );
            drop(conn);
            continue;
        }
        if cycle == 0 {
            assert!(
                request_trace
                    .iter()
                    .any(|request| request["endpoint"] == "changes"
                        && request["zone"] == SOURCE
                        && request["cursor"] == "current-0"),
                "first asset observation must come from the saved scoped changes cursor"
            );
            assert!(
                !request_trace
                    .iter()
                    .any(|request| request["endpoint"] == "query"
                        && request["query_type"] != "CheckIndexingState"),
                "first asset observation must not use full media inventory"
            );
        }
        if repeated_observation && cycle == 1 {
            let prior = saved_retry.as_ref().expect("cycle-zero retry");
            assert!(
                prior[9].as_i64().unwrap() > before_time,
                "old retry is not due"
            );
            let original = original_equivalence.as_ref().unwrap();
            let mut original_matches = 0;
            let mut observed = Vec::new();
            for encoded in &equivalent_rows {
                let row: Value = serde_json::from_str(encoded).unwrap();
                for index in 1..=10 {
                    assert_eq!(
                        row[index], original[index],
                        "source, policy and manifest field {index}"
                    );
                }
                assert_eq!(row[1], account);
                assert_eq!(row[2], provider);
                assert_eq!(row[7], expected_pass);
                assert_eq!(row[8], "asset-shared");
                assert_eq!(row[5].as_str().unwrap().len(), 64);
                assert_eq!(row[6].as_str().unwrap().len(), 64);
                let manifest: Value = serde_json::from_str(row[10].as_str().unwrap()).unwrap();
                let confirmation: Vec<u8> =
                    serde_json::from_value(manifest["decision"]["confirmation"].clone()).unwrap();
                let source: Value = serde_json::from_slice(&confirmation).unwrap();
                let master = source["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| {
                        record["recordName"] == "shared" && record["recordType"] == "CPLMaster"
                    })
                    .unwrap();
                let child = source["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| {
                        record["recordName"] == "asset-shared" && record["recordType"] == "CPLAsset"
                    })
                    .unwrap();
                assert_eq!(master["zoneID"], zone(SOURCE));
                assert_eq!(child["zoneID"], zone(SOURCE));
                assert_eq!(master["recordChangeTag"], "ct-master");
                assert_eq!(child["recordChangeTag"], "ct-asset");
                assert_eq!(
                    child["fields"]["masterRef"]["value"]["recordName"],
                    "shared"
                );
                assert_eq!(
                    child["fields"]["masterRef"]["value"]["zoneID"],
                    zone(SOURCE)
                );
                assert_eq!(
                    master["fields"]["resOriginalRes"]["value"]["downloadURL"],
                    format!("{}/still", server.uri())
                );
                assert_eq!(
                    master["fields"]["resOriginalRes"]["value"]["fileChecksum"],
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still))
                );
                assert_eq!(
                    master["fields"]["resOriginalRes"]["value"]["size"],
                    still.len()
                );
                assert_eq!(
                    master["fields"]["resOriginalVidComplRes"]["value"]["downloadURL"],
                    format!("{}/expired-movie", server.uri())
                );
                assert_eq!(
                    master["fields"]["resOriginalVidComplRes"]["value"]["fileChecksum"],
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
                );
                assert_eq!(
                    master["fields"]["resOriginalVidComplRes"]["value"]["size"],
                    movie.len()
                );
                if row[0] == original[0] {
                    original_matches += 1;
                    assert_eq!(row[11], original[11], "older attempt must remain unchanged");
                    assert_eq!(
                        row[12], original[12],
                        "older deadline must remain unchanged"
                    );
                }
                observed.push(json!({"generation":row[0], "attempts":row[11], "deadline":row[12]}));
            }
            assert_eq!(original_matches, 1, "preserve the original generation");
            assert!(
                retry_rows
                    .iter()
                    .any(|encoded| serde_json::from_str::<Value>(encoded).unwrap() == *prior),
                "the exact original retry owner remains retained"
            );
            let repeated_gets = movie_gets.load(Ordering::SeqCst) - before_requests;
            assert!(
                crate::state::db::retry_clock::now_for_path(&database) < prior[9].as_i64().unwrap(),
                "the repeated-observation cycle must finish before the old deadline"
            );
            if approved_wait {
                assert_eq!(
                    repeated_gets, 0,
                    "identical observation must honor the existing future wait"
                );
                assert_eq!(
                    equivalent_rows.len(),
                    2,
                    "both source observations remain represented"
                );
                let second: Vec<Value> = equivalent_rows
                    .iter()
                    .map(|encoded| serde_json::from_str::<Value>(encoded).unwrap())
                    .filter(|row| row[0] != original[0])
                    .collect();
                assert_eq!(second.len(), 1);
                assert_eq!(second[0][11], 0, "a shared wait is not a transfer attempt");
                let target_generation = second[0][0].as_str().unwrap();
                let old_generation = original[0].as_str().unwrap();
                let path_key = serde_json::to_string(
                    &crate::state::db::provider_selection::SelectionPath::from_path(&movie_path),
                )
                .unwrap();
                // Compact receipts are trusted runtime attestations of the
                // canonical records, not retained raw provider page bytes.
                type MovieEpochRow = (
                    String,
                    String,
                    String,
                    String,
                    String,
                    i64,
                    i64,
                    String,
                    String,
                    String,
                    String,
                    String,
                    i64,
                    String,
                );
                let epochs: Vec<MovieEpochRow> = conn.prepare(
                    "SELECT generation,pass_key,child,version_size,path,attempt,deadline,marker_hash,route_id,state_id,master,checksum,size_bytes,url FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original' ORDER BY deadline,attempt"
                ).unwrap().query_map(rusqlite::params![old_generation,expected_pass], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?,r.get(13)?))).unwrap().collect::<Result<_,_>>().unwrap();
                assert_eq!(
                    epochs.len(),
                    1,
                    "one retained real 410 epoch for this movie"
                );
                let epoch = &epochs[0];
                assert_eq!(epoch.0, old_generation);
                assert_eq!(epoch.1, expected_pass);
                assert_eq!(epoch.2, "asset-shared");
                assert_eq!(epoch.3, "live_original");
                assert_eq!(epoch.4, path_key);
                assert_eq!(epoch.5, 1);
                assert_eq!(epoch.6, original[12].as_i64().unwrap());
                assert!(epoch.6 > crate::state::db::retry_clock::now_for_path(&database));
                assert_eq!(epoch.7.len(), 64);
                assert_eq!(epoch.8.len(), 64);
                assert_eq!(epoch.9, "asset-shared");
                assert_eq!(epoch.10, "shared");
                assert_eq!(
                    epoch.11,
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
                );
                assert_eq!(epoch.12, i64::try_from(movie.len()).unwrap());
                assert_eq!(epoch.13, format!("{}/expired-movie", server.uri()));
                type SharedWaitRow = (
                    String,
                    String,
                    String,
                    String,
                    String,
                    String,
                    i64,
                    i64,
                    String,
                    String,
                    String,
                    String,
                );
                let waits: Vec<SharedWaitRow> = conn.prepare(
                    "SELECT generation,pass_key,child,version_size,path,source_generation,source_attempt,source_deadline,source_marker,source_route,target_route,proof_hash FROM provider_410_shared_waits WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' ORDER BY version_size,path"
                ).unwrap().query_map(rusqlite::params![target_generation,expected_pass], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?))).unwrap().collect::<Result<_,_>>().unwrap();
                assert_eq!(waits.len(), 1, "one durable wait for the exact movie");
                let wait = &waits[0];
                assert_eq!(
                    (&wait.0, &wait.1, &wait.2, &wait.3, &wait.4),
                    (
                        &target_generation.to_owned(),
                        &expected_pass,
                        &"asset-shared".to_owned(),
                        &"live_original".to_owned(),
                        &path_key
                    )
                );
                assert_eq!(wait.5, old_generation);
                assert_eq!(wait.6, epoch.5);
                assert_eq!(wait.7, epoch.6);
                assert_eq!(wait.8, epoch.7);
                assert_eq!(wait.9, epoch.8);
                assert_eq!(wait.11.len(), 64);
                let receipts: Vec<(String,String,Vec<u8>,String)> = conn.prepare(
                    "SELECT generation,route_id,attestation,proof_hash FROM provider_compact_observations WHERE generation IN (?1,?2) AND pass_key=?3 AND child='asset-shared' ORDER BY generation"
                ).unwrap().query_map(rusqlite::params![old_generation,target_generation,expected_pass], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
                assert_eq!(
                    receipts.len(),
                    2,
                    "both observations retain compact attestations"
                );
                let old_receipt = receipts.iter().find(|r| r.0 == old_generation).unwrap();
                let target_receipt = receipts.iter().find(|r| r.0 == target_generation).unwrap();
                assert_eq!(old_receipt.1, wait.9);
                assert_eq!(target_receipt.1, wait.10);
                assert_ne!(old_receipt.1, target_receipt.1);
                let manifest: Value = serde_json::from_str(original[10].as_str().unwrap()).unwrap();
                let confirmed_bytes: Vec<u8> =
                    serde_json::from_value(manifest["decision"]["confirmation"].clone()).unwrap();
                let confirmed: Value = serde_json::from_slice(&confirmed_bytes).unwrap();
                let mut observations = Vec::new();
                for receipt in [&old_receipt, &target_receipt] {
                    assert_eq!(receipt.3.len(), 64);
                    let source: Value = serde_json::from_slice(&receipt.2).unwrap();
                    assert_eq!(source["evidence"], "trusted-runtime-canonical-records");
                    assert_eq!(source["generation"], receipt.0);
                    assert_eq!(source["scope"], original[3]);
                    assert_eq!(source["zone"], zone(SOURCE));
                    assert_eq!(source["pass"], expected_pass);
                    assert_eq!(source["child"], "asset-shared");
                    assert_eq!(source["master"], "shared");
                    assert_eq!(
                        source["confirmation_hash"],
                        format!("{:x}", Sha256::digest(&confirmed_bytes))
                    );
                    for (name, key) in [
                        ("shared", "master_record"),
                        ("asset-shared", "child_record"),
                    ] {
                        let current = confirmed["records"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|r| r["recordName"] == name)
                            .unwrap();
                        assert_eq!(
                            &source[key], current,
                            "complete canonical observation and confirmation records"
                        );
                    }
                    observations.push(source);
                }
                assert_eq!(
                    observations[0]["master_record"],
                    observations[1]["master_record"]
                );
                assert_eq!(
                    observations[0]["child_record"],
                    observations[1]["child_record"]
                );
                assert_ne!(
                    observations[0]["master_invocation"], observations[1]["master_invocation"],
                    "observations require distinct accepted changes invocations"
                );
                linked_generation = Some(target_generation.to_owned());
                for encoded in &equivalent_rows {
                    let row: Value = serde_json::from_str(encoded).unwrap();
                    let evidence: (bool, bool) = conn.query_row(
                        "SELECT sealed,length(seal_hash)=64 FROM provider_active_generations WHERE id=?1",
                        [row[0].as_str().unwrap()], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
                    assert_eq!(
                        evidence,
                        (true, true),
                        "both observations retain checkpoint evidence"
                    );
                }
                let cursor: String = conn
                    .query_row(
                        "SELECT value FROM metadata WHERE key=?1",
                        [sync_token_key(SOURCE)],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(cursor, "current-2");
                eprintln!(
                    "ISSUE845_SHARED_WAIT_REACHED cycle=1 old_attempts=1 new_attempts=0 movie_410=0 independent_gets=1 observations=2"
                );
            } else {
                assert!(repeated_gets <= 2, "bounded GETs for repeated observation");
            }
            assert!(
                request_trace
                    .iter()
                    .any(|request| request["endpoint"] == "changes"
                        && request["zone"] == SOURCE
                        && request["cursor"]
                            == if approved_wait {
                                "current-0"
                            } else {
                                "current-1"
                            }),
                "the repeated observation must reach the intended Shared cursor"
            );
            eprintln!(
                "ISSUE845_REPEAT_DIAG cycle=1 source_equivalent=true old_retry_future=true cycle_movie_410={repeated_gets} new_transfer_before_old_deadline={} retry_owners={observed:?} get_generation_attribution=unavailable",
                repeated_gets > 0
            );
            drop(conn);
            continue;
        }
        if approved_wait && repeated_observation && cycle == 2 {
            let previous = original_equivalence.as_ref().unwrap();
            let linked = linked_generation
                .as_deref()
                .expect("second observation from cycle one");
            assert_eq!(
                equivalent_rows.len(),
                2,
                "failed due transfer keeps both observations"
            );
            let mut total_attempts = 0_i64;
            let mut generations = std::collections::BTreeSet::new();
            for encoded in &equivalent_rows {
                let row: Value = serde_json::from_str(encoded).unwrap();
                for index in 1..=10 {
                    assert_eq!(
                        row[index], previous[index],
                        "due retry preserves exact source field {index}"
                    );
                }
                let generation = row[0].as_str().unwrap();
                assert!(generations.insert(generation.to_owned()));
                assert!(generation == previous[0].as_str().unwrap() || generation == linked);
                total_attempts += row[11].as_i64().unwrap();
            }
            assert_eq!(generations.len(), 2);
            assert_eq!(
                total_attempts, 2,
                "one actual due retry increments one owner once"
            );
            assert_due_movie_epoch(
                &conn,
                initial_refresh_wait.as_ref().unwrap(),
                initial_movie_epoch.as_deref().unwrap(),
                &generations,
                (
                    &expected_pass,
                    &movie_path,
                    &database,
                    &server.uri(),
                    account,
                    provider,
                ),
            );
            let requests = movie_gets.load(Ordering::SeqCst) - before_requests;
            assert_eq!(
                requests, 1,
                "one due attempt may not repeat the identical future-wait URL"
            );
            assert_eq!(still_gets.load(Ordering::SeqCst), 1);
            assert_eq!(independent_gets.load(Ordering::SeqCst), 1);
            eprintln!(
                "ISSUE845_SHARED_WAIT_REACHED cycle=2 observations=2 total_attempts=2 movie_410=1 still_gets=1 independent_gets=1"
            );
            drop(conn);
            continue;
        }
        assert_eq!(
            retry_rows.len(),
            1,
            "exact retained retry owner on cycle {cycle}"
        );
        let retry: Value = serde_json::from_str(&retry_rows[0]).unwrap();
        assert!(!retry[0].as_str().unwrap().is_empty());
        assert_eq!(retry[1], account);
        assert_eq!(retry[2], provider);
        let retry_scope: Value = serde_json::from_str(retry[3].as_str().unwrap()).unwrap();
        assert_eq!(
            retry_scope,
            json!({
                "container":"com.apple.photos.cloud","database":"shared",
                "environment":"production","format":1,"realm":"com","zone":zone(SOURCE)
            })
        );
        assert_eq!(retry[4], expected_pass);
        assert_eq!(retry[5], "asset-shared");
        assert_eq!(retry[6], "admitted");
        assert_eq!(retry[7], "current_confirmation_unavailable");
        let expected_attempts = if cycle == 2 { 2 } else { 1 };
        assert_eq!(retry[8], expected_attempts);
        let deadline = retry[9].as_i64().unwrap();
        let after_time = crate::state::db::retry_clock::now_for_path(&database);
        if cycle == 1 {
            assert!(
                deadline > after_time,
                "quiet cycle must remain under backoff"
            );
            assert_eq!(
                Some(&retry),
                saved_retry.as_ref(),
                "quiet reopen preserves owner and backoff exactly"
            );
        } else {
            let delay = if cycle == 2 { 7200 } else { 3600 };
            assert!(
                deadline >= before_time + delay && deadline <= after_time + delay,
                "deadline must be set by this attempt: {retry}"
            );
            if let Some(previous) = &saved_retry {
                for index in 0..8 {
                    assert_eq!(
                        retry[index], previous[index],
                        "retry identity field {index}"
                    );
                }
            }
        }
        if (approved_wait || changed_url) && cycle == 0 {
            type MovieEvidenceRow = (
                String,
                String,
                String,
                i64,
                i64,
                String,
                String,
                String,
                String,
                String,
                String,
                i64,
                String,
            );
            let evidence: Vec<MovieEvidenceRow> = conn.prepare(
                "SELECT e.generation,e.pass_key,e.path,e.attempt,e.deadline,e.marker_hash,e.url,w.generation,w.pass_key,w.path,w.source_id,w.deadline,w.proof_hash FROM provider_410_failure_epochs e JOIN provider_refreshed_waits w ON w.generation=e.generation AND w.pass_key=e.pass_key AND w.child=e.child AND w.version_size=e.version_size AND w.path=e.path AND w.source_kind='original' WHERE e.generation=?1 AND e.pass_key=?2 AND e.child='asset-shared' AND e.version_size='live_original' ORDER BY e.attempt,e.deadline"
            ).unwrap().query_map(rusqlite::params![retry[0].as_str().unwrap(), expected_pass], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?))).unwrap().collect::<Result<_,_>>().unwrap();
            assert_eq!(evidence.len(), 1, "exact same-owner 410 and refresh wait");
            let (
                generation,
                pass,
                path,
                attempt,
                epoch_deadline,
                marker,
                url,
                wait_generation,
                wait_pass,
                wait_path,
                source_id,
                wait_deadline,
                proof,
            ) = &evidence[0];
            assert_eq!(generation, retry[0].as_str().unwrap());
            assert_eq!(pass, &expected_pass);
            assert_eq!(
                serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(path)
                    .unwrap()
                    .to_path(),
                movie_path
            );
            assert_eq!(*attempt, 1);
            assert_eq!(*epoch_deadline, deadline);
            assert!(*epoch_deadline > after_time);
            assert_eq!(marker.len(), 64);
            assert_eq!(url, &format!("{}/expired-movie", server.uri()));
            assert_eq!(
                (wait_generation, wait_pass, wait_path),
                (generation, pass, path)
            );
            assert_eq!(*wait_deadline, *epoch_deadline);
            let receipt: Vec<u8> = conn.query_row(
                "SELECT receipt FROM provider_refreshed_waits WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original' AND path=?3 AND source_kind='original' AND source_id=?4",
                rusqlite::params![generation, pass, path, source_id], |r| r.get(0),
            ).unwrap();
            let retained: Value = serde_json::from_slice(&receipt).unwrap();
            let invocation: [u8; 16] =
                serde_json::from_value(retained["invocation"].clone()).unwrap();
            let invocation = uuid::Uuid::from_bytes(invocation);
            assert!(!invocation.is_nil(), "retained lookup invocation is nonnil");
            assert_eq!(
                source_id,
                &format!("{marker}:{invocation}"),
                "wait references the exact epoch and retained lookup invocation"
            );
            assert_eq!(proof.len(), 64);
            initial_refresh_wait = Some((
                marker.clone(),
                *epoch_deadline,
                source_id.clone(),
                proof.clone(),
            ));
            let saved: String = conn.query_row(
                "SELECT json_array(e.generation,e.pass_key,e.child,e.version_size,e.path,e.attempt,e.deadline,e.marker_hash,e.url,e.route_id,g.account_key,g.provider_key,g.scope) FROM provider_410_failure_epochs e JOIN provider_active_generations g ON g.id=e.generation WHERE e.marker_hash=?1",
                [marker], |r| r.get(0),
            ).unwrap();
            initial_movie_epoch = Some(saved);
        }
        if approved_wait && !repeated_observation && cycle == 2 {
            let original_generation = retry[0].as_str().unwrap().to_owned();
            let generations = std::collections::BTreeSet::from([original_generation]);
            assert_due_movie_epoch(
                &conn,
                initial_refresh_wait.as_ref().unwrap(),
                initial_movie_epoch.as_deref().unwrap(),
                &generations,
                (
                    &expected_pass,
                    &movie_path,
                    &database,
                    &server.uri(),
                    account,
                    provider,
                ),
            );
        }
        if approved_wait && !repeated_observation && cycle == 1 {
            let waits: Vec<(String,i64,String,String)> = conn.prepare(
                "SELECT e.marker_hash,e.deadline,w.source_id,w.proof_hash FROM provider_410_failure_epochs e JOIN provider_refreshed_waits w ON w.generation=e.generation AND w.pass_key=e.pass_key AND w.child=e.child AND w.version_size=e.version_size AND w.path=e.path AND w.source_kind='original' WHERE e.generation=?1 AND e.pass_key=?2 AND e.child='asset-shared' AND e.version_size='live_original' ORDER BY e.attempt,e.deadline"
            ).unwrap().query_map(rusqlite::params![retry[0].as_str().unwrap(), expected_pass], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
            assert_eq!(
                waits.as_slice(),
                std::slice::from_ref(initial_refresh_wait.as_ref().unwrap()),
                "quiet reopen retains the exact original movie epoch and wait"
            );
        }
        saved_retry = Some(retry);
        drop(conn);
        let requests = movie_gets.load(Ordering::SeqCst) - before_requests;
        let expected_requests = if cycle == 1 {
            0
        } else if (approved_wait && (cycle == 0 || !repeated_observation))
            || (changed_url && cycle == 0)
        {
            1
        } else {
            2
        };
        assert_eq!(
            requests, expected_requests,
            "per-cycle 410 requests on cycle {cycle}"
        );
        eprintln!(
            "ISSUE845_REACHED cycle={cycle} cycle_still_gets={still_requests} cycle_movie_410={requests} pending_paths=1 still_receipt=1 movie_receipts=0 retry_attempts={expected_attempts} retry_generation={} retry_deadline={deadline} pass={expected_pass} exact_tree=true",
            saved_retry.as_ref().unwrap()[0]
        );
    }
    server.verify().await;
}

fn assert_due_movie_epoch(
    conn: &rusqlite::Connection,
    initial_refresh_wait: &(String, i64, String, String),
    initial_movie_epoch: &str,
    generations: &std::collections::BTreeSet<String>,
    expected: (&str, &std::path::Path, &std::path::Path, &str, &str, &str),
) {
    let (expected_pass, movie_path, database, server_uri, account, provider) = expected;
    let (old_marker, _, _, _) = initial_refresh_wait;
    let old_epoch: String = conn.query_row(
        "SELECT json_array(e.generation,e.pass_key,e.child,e.version_size,e.path,e.attempt,e.deadline,e.marker_hash,e.url,e.route_id,g.account_key,g.provider_key,g.scope) FROM provider_410_failure_epochs e JOIN provider_active_generations g ON g.id=e.generation WHERE e.marker_hash=?1",
        [old_marker], |r| r.get(0),
    ).unwrap();
    assert_eq!(old_epoch, initial_movie_epoch);
    type DueEpochRow = (
        String,
        String,
        String,
        i64,
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
    );
    let new_epochs: Vec<DueEpochRow> = conn.prepare(
        "SELECT e.generation,e.pass_key,e.path,e.attempt,e.deadline,e.marker_hash,e.url,g.account_key,g.provider_key,g.scope,e.route_id FROM provider_410_failure_epochs e JOIN provider_active_generations g ON g.id=e.generation WHERE e.pass_key=?1 AND e.child='asset-shared' AND e.version_size='live_original' AND e.marker_hash<>?2 ORDER BY e.generation,e.attempt,e.deadline"
    ).unwrap().query_map(rusqlite::params![expected_pass,old_marker], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?))).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(new_epochs.len(), 1, "exactly one actual due 410 epoch");
    let due = &new_epochs[0];
    assert!(generations.contains(&due.0));
    assert_eq!(due.1, expected_pass);
    assert_eq!(
        serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(&due.2)
            .unwrap()
            .to_path(),
        movie_path
    );
    assert!(due.3 > 0);
    assert!(due.4 > crate::state::db::retry_clock::now_for_path(database));
    assert_eq!(due.5.len(), 64);
    assert_eq!(due.6, format!("{}/expired-movie", server_uri));
    assert_eq!(due.7, account);
    assert_eq!(due.8, provider);
    assert_eq!(
        serde_json::from_str::<Value>(&due.9).unwrap(),
        json!({"container":"com.apple.photos.cloud","database":"shared","environment":"production","format":1,"realm":"com","zone":zone(SOURCE)})
    );
    assert_eq!(due.10.len(), 64);
    let owner_retry: (i64,i64) = conn.query_row(
        "SELECT attempts,next_retry_at FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child='asset-shared'",
        rusqlite::params![due.0,due.1], |r| Ok((r.get(0)?,r.get(1)?)),
    ).unwrap();
    assert_eq!(
        owner_retry,
        (due.3, due.4),
        "new epoch belongs to its exact durable owner"
    );
}

// Independent in-memory provider scenario. This does not call run_cycle or
// inspect persisted continuation/checkpoint state, and makes no cursor claim.
#[derive(Clone)]
struct FinalizationAbortSession(Issue845Session);

#[async_trait::async_trait]
impl PhotosSession for FinalizationAbortSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(request["zones"][0]["zoneID"], zone(SOURCE));
            self.0
                .0
                .requests
                .lock()
                .unwrap()
                .push((url.into(), request));
            return Ok(json!({"zones":[{"zoneID":zone(SOURCE),
                "records":self.0.0.records_for(SOURCE),"syncToken":"synthetic-page-end","moreComing":false}]}));
        }
        self.0.post(url, body, headers).await
    }
    async fn current_source_authority(
        &self,
    ) -> Option<crate::icloud::photos::session::SourceAuthority> {
        crate::icloud::photos::session::SourceAuthority::synthetic(
            &baseline_owner(),
            "https://synthetic.example",
        )
    }
    async fn post_changes_with_evidence(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
        invocation: crate::icloud::photos::session::InvocationToken,
    ) -> anyhow::Result<crate::icloud::photos::session::ChangesResponse> {
        let response = self.post(url, body.clone(), headers).await?;
        crate::icloud::photos::session::ChangesResponse::synthetic(
            &baseline_owner(),
            "https://synthetic.example",
            url,
            &body,
            headers,
            &serde_json::to_vec(&response)?,
            invocation,
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn issue845_independent_abort_after_real_selected_receipt_preserves_guard_and_ticket() {
    use base64::Engine as _;
    use futures_util::future::{AbortHandle, Abortable};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server =
        crate::test_helpers::start_wiremock_or_skip("independent selected finalization abort")
            .await
            .expect("synthetic CDN");
    let still = include_bytes!("../../../../../../tests/data/media/apple-live.heic").as_slice();
    let movie = include_bytes!("../../../../../../tests/data/media/apple-live.mov").as_slice();
    let old_gets = Arc::new(AtomicUsize::new(0));
    let fresh_gets = Arc::new(AtomicUsize::new(0));
    let counter = old_gets.clone();
    Mock::given(method("GET"))
        .and(path("/expired-movie"))
        .respond_with(move |_: &wiremock::Request| {
            counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(410)
        })
        .mount(&server)
        .await;
    let counter = fresh_gets.clone();
    Mock::given(method("GET"))
        .and(path("/fresh-movie"))
        .respond_with(move |_: &wiremock::Request| {
            counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_bytes(movie)
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/still"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(still))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let owner = baseline_owner();
    let (account, provider) = owner.authenticated_keys().unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
    let mut records = pair(
        "shared",
        "separate.HEIC",
        &format!("{}/still", server.uri()),
        22,
    );
    records[0]["fields"]["itemType"] = json!({"value":"public.heic"});
    records[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
    records[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(still.len());
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still)));
    records[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/expired-movie",server.uri()),"size":movie.len(),"fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))}});
    records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    for record in &mut records {
        record["zoneID"] = zone(SOURCE);
        if record["recordType"] == "CPLAsset" {
            record["fields"]["masterRef"]["value"]["zoneID"] = zone(SOURCE);
        }
    }
    let requests = Arc::default();
    let events = Arc::default();
    let session = FinalizationAbortSession(Issue845Session(
        CrossZoneSession {
            cycle: 1,
            records: Arc::new(records),
            requests,
        },
        false,
        Some((old_gets.clone(), format!("{}/fresh-movie", server.uri()))),
        events,
        false,
        false,
    ));
    let capture =
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com");
    let album = issue845_album(&session, SOURCE, &capture);
    let expected_scope = album.selection_scope();
    let expected_pass = format!(
        "{:x}",
        Sha256::digest(
            json!({"kind":"Unfiled","scope":expected_scope,"excludes":[]})
                .to_string()
                .as_bytes()
        )
    );
    let passes = [AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    }];
    let mut config = download::DownloadConfig::test_default();
    config.directory = Arc::from(root.as_path());
    config.folder_structure = "%Y/%m/%d".into();
    config.folder_structure_albums = Arc::from("%Y/%m/%d");
    config.folder_structure_smart_folders = Arc::from("%Y/%m/%d");
    config.media.live_photos = true;
    config.concurrent_downloads = 1;
    config.retry.max_retries = 0;
    config.library = Arc::from(SOURCE);
    config.state_db = Some(db.clone());
    // The literal is a local synthetic provider input, not a read of stored state.
    config.sync_mode = download::SyncMode::Incremental {
        zone_sync_token: "synthetic-in-memory-input".into(),
    };
    let date = crate::sync_loop::test_support::run_cycle_expected_date_dir();
    let movie_path = root.join(date).join("separate_HEVC.MOV");
    let (entered, release) =
        crate::download::pipeline::real_410_abort_hook::install_refreshed_finalized(
            movie_path.clone(),
        );
    let (abort, registration) = AbortHandle::new_pair();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let mut future = Box::pin(Abortable::new(
        download::download_photos_with_sync(
            &client,
            &passes,
            Arc::new(config),
            download::DownloadControls::download_hidden(),
            cancel,
        ),
        registration,
    ));
    let held = tokio::select! {ready=entered=>ready.expect("actual selected receipt persisted"),early=&mut future=>{let _ = early; panic!("selected download ended before receipt hook")},()=tokio::time::sleep(std::time::Duration::from_secs(10))=>panic!("selected finalization hook not reached")};
    assert_eq!(old_gets.load(Ordering::SeqCst), 1);
    assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&movie_path).unwrap(), movie);
    let movie_stamp = std::fs::metadata(&movie_path).unwrap().modified().unwrap();
    let (id, proof, generation, attempts, receipt, original_epoch): (
        String,
        String,
        String,
        i64,
        String,
        String,
    ) = {
        let conn = db
            .acquire_lock("independent synthetic selected receipt")
            .unwrap();
        let rows:Vec<(String,String,String,String,String,String)>=conn.prepare("SELECT t.id,t.proof_hash,t.generation,t.phase,t.path,g.scope FROM provider_refreshed_transfer t JOIN provider_active_generations g ON g.id=t.generation WHERE t.pass_key=?1 AND t.child='asset-shared' AND t.version_size='live_original' AND g.account_key=?2 AND g.provider_key=?3 ORDER BY t.id").unwrap().query_map(rusqlite::params![expected_pass,account,provider],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.3, "inflight");
        assert_eq!(row.1.len(), 64);
        assert_eq!(
            serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(&row.4)
                .unwrap()
                .to_path(),
            movie_path
        );
        assert_eq!(
            serde_json::from_str::<Value>(&row.5).unwrap(),
            json!({"container":"com.apple.photos.cloud","database":"shared","environment":"production","format":1,"realm":"com","zone":zone(SOURCE)})
        );
        let receipt:String=conn.query_row("SELECT json_array(r.account_key,r.provider_key,r.scope,r.library,r.child,r.master,r.origin_pass,r.version_size,r.provider_checksum,r.provider_size,r.native_path,r.proof_hash) FROM download_receipts r JOIN provider_active_destinations w ON w.generation=?1 AND w.pass_key=r.origin_pass AND w.child=r.child AND w.master=r.master AND w.version_size=r.version_size AND w.path=r.native_path AND w.checksum=r.provider_checksum AND w.size_bytes=r.provider_size WHERE r.account_key=?2 AND r.provider_key=?3 AND r.scope=?4 AND r.library=?5 AND r.child='asset-shared' AND r.version_size='live_original'",rusqlite::params![row.2,account,provider,row.5,SOURCE],|r|r.get(0)).unwrap();
        let archived: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_refreshed_success_receipts WHERE dispatch_id=?1",
                [&row.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archived, 0);
        let attempts:i64=conn.query_row("SELECT download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap();
        let original_epoch:String=conn.query_row("SELECT json_array(generation,pass_key,child,version_size,path,attempt,deadline,marker_hash,url) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original' AND path=?3",rusqlite::params![row.2,expected_pass,row.4],|r|r.get(0)).unwrap();
        (
            row.0.clone(),
            row.1.clone(),
            row.2.clone(),
            attempts,
            receipt,
            original_epoch,
        )
    };
    assert!(
        !crate::download::pipeline::real_410_abort_hook::can_lock(
            &movie_path,
            std::time::Duration::from_millis(100)
        )
        .await
    );
    abort.abort();
    assert!(future.as_mut().await.is_err());
    drop(future);
    assert!(held.load(Ordering::SeqCst));
    assert!(
        !crate::download::pipeline::real_410_abort_hook::can_lock(
            &movie_path,
            std::time::Duration::from_millis(100)
        )
        .await
    );
    release.send(()).unwrap();
    assert!(
        crate::download::pipeline::real_410_abort_hook::can_lock(
            &movie_path,
            std::time::Duration::from_secs(10)
        )
        .await
    );
    {
        let conn = db
            .acquire_lock("independent selected terminal after abort")
            .unwrap();
        let (phase,saved_proof,receipt_id):(String,String,String)=conn.query_row("SELECT phase,proof_hash,success_receipt_id FROM provider_refreshed_transfer WHERE id=?1 AND generation=?2",rusqlite::params![id,generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(phase, "published");
        assert_eq!(saved_proof, proof);
        let count:i64=conn.query_row("SELECT count(*) FROM provider_refreshed_success_receipts WHERE dispatch_id=?1 AND receipt_id=?2 AND length(proof_hash)=64",rusqlite::params![id,receipt_id],|r|r.get(0)).unwrap();
        assert_eq!(count, 1);
        let actual:String=conn.query_row("SELECT json_array(r.account_key,r.provider_key,r.scope,r.library,r.child,r.master,r.origin_pass,r.version_size,r.provider_checksum,r.provider_size,r.native_path,r.proof_hash) FROM download_receipts r JOIN provider_active_destinations w ON w.generation=?1 AND w.pass_key=r.origin_pass AND w.child=r.child AND w.master=r.master AND w.version_size=r.version_size AND w.path=r.native_path AND w.checksum=r.provider_checksum AND w.size_bytes=r.provider_size WHERE r.account_key=?2 AND r.provider_key=?3 AND r.library=?4 AND r.child='asset-shared' AND r.version_size='live_original'",rusqlite::params![generation,account,provider,SOURCE],|r|r.get(0)).unwrap();
        assert_eq!(actual, receipt);
        let after:i64=conn.query_row("SELECT download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap();
        assert_eq!(after, attempts);
        let after_epoch:String=conn.query_row("SELECT json_array(generation,pass_key,child,version_size,path,attempt,deadline,marker_hash,url) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original'",rusqlite::params![generation,expected_pass],|r|r.get(0)).unwrap();
        assert_eq!(after_epoch, original_epoch);
    }
    assert!(held.load(Ordering::SeqCst));
    assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
    assert_eq!(old_gets.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&movie_path).unwrap(), movie);
    assert_eq!(
        std::fs::metadata(&movie_path).unwrap().modified().unwrap(),
        movie_stamp
    );
    eprintln!(
        "ISSUE845_INDEPENDENT_FINALIZATION_ABORT_REACHED real_receipt=true same_guard=true held=true ticket_published=true repeat_gets=0 checkpoint_claim=none"
    );
}

#[tokio::test]
async fn issue845_receipted_inflight_reopen_reaches_selected_recovery_scheduler() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server =
        crate::test_helpers::start_wiremock_or_skip("independent selected finalization abort")
            .await
            .expect("synthetic CDN");
    let still = include_bytes!("../../../../../../tests/data/media/apple-live.heic").as_slice();
    let movie = include_bytes!("../../../../../../tests/data/media/apple-live.mov").as_slice();
    let old_gets = Arc::new(AtomicUsize::new(0));
    let fresh_gets = Arc::new(AtomicUsize::new(0));
    let counter = old_gets.clone();
    Mock::given(method("GET"))
        .and(path("/expired-movie"))
        .respond_with(move |_: &wiremock::Request| {
            counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(410)
        })
        .mount(&server)
        .await;
    let counter = fresh_gets.clone();
    Mock::given(method("GET"))
        .and(path("/fresh-movie"))
        .respond_with(move |_: &wiremock::Request| {
            counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_bytes(movie)
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/still"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(still))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let owner = baseline_owner();
    let (account, provider) = owner.authenticated_keys().unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
    let mut records = pair(
        "shared",
        "separate.HEIC",
        &format!("{}/still", server.uri()),
        22,
    );
    records[0]["fields"]["itemType"] = json!({"value":"public.heic"});
    records[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
    records[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(still.len());
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still)));
    records[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{"downloadURL":format!("{}/expired-movie",server.uri()),"size":movie.len(),"fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))}});
    records[0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    for record in &mut records {
        record["zoneID"] = zone(SOURCE);
        if record["recordType"] == "CPLAsset" {
            record["fields"]["masterRef"]["value"]["zoneID"] = zone(SOURCE);
        }
    }
    let requests = Arc::default();
    let events = Arc::default();
    let session = FinalizationAbortSession(Issue845Session(
        CrossZoneSession {
            cycle: 1,
            records: Arc::new(records),
            requests,
        },
        false,
        Some((old_gets.clone(), format!("{}/fresh-movie", server.uri()))),
        events,
        false,
        false,
    ));
    let capture =
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com");
    let album = issue845_album(&session, SOURCE, &capture);
    let expected_scope = album.selection_scope();
    let expected_pass = format!(
        "{:x}",
        Sha256::digest(
            json!({"kind":"Unfiled","scope":expected_scope,"excludes":[]})
                .to_string()
                .as_bytes()
        )
    );
    let passes = [AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    }];
    let mut config = download::DownloadConfig::test_default();
    config.directory = Arc::from(root.as_path());
    config.folder_structure = "%Y/%m/%d".into();
    config.folder_structure_albums = Arc::from("%Y/%m/%d");
    config.folder_structure_smart_folders = Arc::from("%Y/%m/%d");
    config.media.live_photos = true;
    config.concurrent_downloads = 1;
    config.retry.max_retries = 0;
    config.library = Arc::from(SOURCE);
    config.state_db = Some(db.clone());
    // The literal is a local synthetic provider input, not a read of stored state.
    config.sync_mode = download::SyncMode::Incremental {
        zone_sync_token: "synthetic-in-memory-input".into(),
    };
    let date = crate::sync_loop::test_support::run_cycle_expected_date_dir();
    let movie_path = root.join(date).join("separate_HEVC.MOV");
    let (entered, release) =
        crate::download::pipeline::real_410_abort_hook::install_refreshed_abandon(
            movie_path.clone(),
        );
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let mut resumed_config = config.clone();
    resumed_config.state_db = None;
    let mut future = Box::pin(download::download_photos_with_sync(
        &client,
        &passes,
        Arc::new(config),
        download::DownloadControls::download_hidden(),
        cancel,
    ));
    let held = tokio::select! {ready=entered=>ready.expect("actual selected receipt persisted"),early=&mut future=>{let _ = early; panic!("selected download ended before receipt hook")},()=tokio::time::sleep(std::time::Duration::from_secs(10))=>panic!("selected finalization hook not reached")};
    assert_eq!(old_gets.load(Ordering::SeqCst), 1);
    assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&movie_path).unwrap(), movie);
    let movie_stamp = std::fs::metadata(&movie_path).unwrap().modified().unwrap();
    let (id, proof, generation, attempts, receipt, original_epoch): (
        String,
        String,
        String,
        i64,
        String,
        String,
    ) = {
        let conn = db
            .acquire_lock("independent synthetic selected receipt")
            .unwrap();
        let rows:Vec<(String,String,String,String,String,String)>=conn.prepare("SELECT t.id,t.proof_hash,t.generation,t.phase,t.path,g.scope FROM provider_refreshed_transfer t JOIN provider_active_generations g ON g.id=t.generation WHERE t.pass_key=?1 AND t.child='asset-shared' AND t.version_size='live_original' AND g.account_key=?2 AND g.provider_key=?3 ORDER BY t.id").unwrap().query_map(rusqlite::params![expected_pass,account,provider],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.3, "inflight");
        assert_eq!(row.1.len(), 64);
        assert_eq!(
            serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(&row.4)
                .unwrap()
                .to_path(),
            movie_path
        );
        assert_eq!(
            serde_json::from_str::<Value>(&row.5).unwrap(),
            json!({"container":"com.apple.photos.cloud","database":"shared","environment":"production","format":1,"realm":"com","zone":zone(SOURCE)})
        );
        let receipt:String=conn.query_row("SELECT json_array(r.account_key,r.provider_key,r.scope,r.library,r.child,r.master,r.origin_pass,r.version_size,r.provider_checksum,r.provider_size,r.native_path,r.proof_hash) FROM download_receipts r JOIN provider_active_destinations w ON w.generation=?1 AND w.pass_key=r.origin_pass AND w.child=r.child AND w.master=r.master AND w.version_size=r.version_size AND w.path=r.native_path AND w.checksum=r.provider_checksum AND w.size_bytes=r.provider_size WHERE r.account_key=?2 AND r.provider_key=?3 AND r.scope=?4 AND r.library=?5 AND r.child='asset-shared' AND r.version_size='live_original'",rusqlite::params![row.2,account,provider,row.5,SOURCE],|r|r.get(0)).unwrap();
        let archived: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_refreshed_success_receipts WHERE dispatch_id=?1",
                [&row.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(archived, 0);
        let attempts:i64=conn.query_row("SELECT download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap();
        let original_epoch:String=conn.query_row("SELECT json_array(generation,pass_key,child,version_size,path,attempt,deadline,marker_hash,url) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original' AND path=?3",rusqlite::params![row.2,expected_pass,row.4],|r|r.get(0)).unwrap();
        (
            row.0.clone(),
            row.1.clone(),
            row.2.clone(),
            attempts,
            receipt,
            original_epoch,
        )
    };
    assert!(
        !crate::download::pipeline::real_410_abort_hook::can_lock(
            &movie_path,
            std::time::Duration::from_millis(100)
        )
        .await
    );
    release.send(()).unwrap();
    let first = future.await.expect("selected interrupted cycle completes");
    assert!(
        first.stats.failed > 0,
        "terminal interruption is retained as failure"
    );
    assert!(held.load(Ordering::SeqCst));
    assert!(
        crate::download::pipeline::real_410_abort_hook::can_lock(
            &movie_path,
            std::time::Duration::from_secs(10)
        )
        .await
    );
    {
        let conn = db
            .acquire_lock("synthetic unfinished selected terminal")
            .unwrap();
        let phase: String = conn
            .query_row(
                "SELECT phase FROM provider_refreshed_transfer WHERE id=?1 AND generation=?2",
                rusqlite::params![id, generation],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "inflight");
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_refreshed_success_receipts WHERE dispatch_id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    drop(passes);
    drop(capture);
    drop(db);
    let reopened = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
    let resumed_capture =
        crate::icloud::photos::inbox::ShadowCapture::new(reopened.clone(), owner.clone(), "com");
    let quiet_session = FinalizationAbortSession(Issue845Session(
        CrossZoneSession {
            cycle: 2,
            records: Arc::new(Vec::new()),
            requests: Arc::default(),
        },
        false,
        None,
        Arc::default(),
        false,
        false,
    ));
    let resumed_passes = [AlbumPass {
        kind: PassKind::Unfiled,
        album: issue845_album(&quiet_session, SOURCE, &resumed_capture),
        exclude_ids: Arc::default(),
    }];
    resumed_config.state_db = Some(reopened.clone());
    let _resumed = download::download_photos_with_sync(
        &client,
        &resumed_passes,
        Arc::new(resumed_config),
        download::DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("reopened production scheduler cycle");
    {
        let conn = reopened
            .acquire_lock("synthetic recovered selected terminal")
            .unwrap();
        let (phase,saved_proof,receipt_id):(String,String,String)=conn.query_row(
            "SELECT phase,proof_hash,success_receipt_id FROM provider_refreshed_transfer WHERE id=?1 AND generation=?2",
            rusqlite::params![id,generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))
        ).unwrap();
        assert_eq!(phase, "published");
        assert_eq!(saved_proof, proof);
        let count:i64=conn.query_row(
            "SELECT count(*) FROM provider_refreshed_success_receipts WHERE dispatch_id=?1 AND receipt_id=?2 AND length(proof_hash)=64",
            rusqlite::params![id,receipt_id],|r|r.get(0)
        ).unwrap();
        assert_eq!(count, 1);
        let (archive, archive_proof): (Vec<u8>, String) = conn.query_row(
            "SELECT receipt,proof_hash FROM provider_refreshed_success_receipts WHERE dispatch_id=?1 AND receipt_id=?2",
            rusqlite::params![id,receipt_id], |r| Ok((r.get(0)?,r.get(1)?)),
        ).unwrap();
        let archived: crate::state::db::download_receipts::DownloadReceipt =
            serde_json::from_slice(&archive).unwrap();
        assert_eq!(serde_json::to_vec(&archived).unwrap(), archive);
        assert_eq!(archived.id().unwrap(), receipt_id);
        assert_eq!(archived.account_key, account);
        assert_eq!(archived.provider_key, provider);
        assert_eq!(archived.library, SOURCE);
        assert_eq!(
            serde_json::from_str::<Value>(&archived.scope).unwrap(),
            json!({"container":"com.apple.photos.cloud","database":"shared","environment":"production","format":1,"realm":"com","zone":zone(SOURCE)})
        );
        assert_eq!(archived.child, "asset-shared");
        assert_eq!(archived.state_id, "asset-shared");
        assert_eq!(archived.master, "shared");
        assert_eq!(archived.origin_pass, expected_pass);
        assert_eq!(archived.version_size, "live_original");
        assert_eq!(archived.native_path.to_path(), movie_path);
        assert_eq!(archived.provider_size, movie.len() as u64);
        assert_eq!(
            archived.provider_checksum,
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
        );
        let movie_checksum = format!("{:x}", Sha256::digest(movie));
        assert_eq!(archived.local_checksum, movie_checksum);
        assert_eq!(archived.source_checksum, movie_checksum);
        let (stored_source, stored_local): (String, String) = conn
            .query_row(
                "SELECT source_checksum,local_checksum FROM download_receipts WHERE id=?1",
                [&receipt_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored_source, archived.source_checksum);
        assert_eq!(stored_local, archived.local_checksum);
        let expected_archive_proof = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    "authenticated-refreshed-success-receipt-v1",
                    &id,
                    &receipt_id,
                    &archive,
                ))
                .unwrap()
            )
        );
        assert_eq!(archive_proof, expected_archive_proof);
        let actual:String=conn.query_row("SELECT json_array(r.account_key,r.provider_key,r.scope,r.library,r.child,r.master,r.origin_pass,r.version_size,r.provider_checksum,r.provider_size,r.native_path,r.proof_hash) FROM download_receipts r JOIN provider_active_destinations w ON w.generation=?1 AND w.pass_key=r.origin_pass AND w.child=r.child AND w.master=r.master AND w.version_size=r.version_size AND w.path=r.native_path AND w.checksum=r.provider_checksum AND w.size_bytes=r.provider_size WHERE r.account_key=?2 AND r.provider_key=?3 AND r.library=?4 AND r.child='asset-shared' AND r.version_size='live_original'",rusqlite::params![generation,account,provider,SOURCE],|r|r.get(0)).unwrap();
        assert_eq!(actual, receipt);
        let after:i64=conn.query_row("SELECT download_attempts FROM assets WHERE library=?1 AND id='asset-shared' AND version_size='live_original'",[SOURCE],|r|r.get(0)).unwrap();
        assert_eq!(after, attempts);
        let after_epoch:String=conn.query_row("SELECT json_array(generation,pass_key,child,version_size,path,attempt,deadline,marker_hash,url) FROM provider_410_failure_epochs WHERE generation=?1 AND pass_key=?2 AND child='asset-shared' AND version_size='live_original'",rusqlite::params![generation,expected_pass],|r|r.get(0)).unwrap();
        assert_eq!(after_epoch, original_epoch);
    }
    assert_eq!(old_gets.load(Ordering::SeqCst), 1);
    assert_eq!(fresh_gets.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(&movie_path).unwrap(), movie);
    assert_eq!(
        std::fs::metadata(&movie_path).unwrap().modified().unwrap(),
        movie_stamp
    );
    eprintln!(
        "ISSUE845_RECEIPTED_RECOVERY_REOPEN_REACHED terminal=published archive=1 repeat_gets=0"
    );
}
