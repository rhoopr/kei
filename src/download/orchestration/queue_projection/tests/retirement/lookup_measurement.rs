//! Explicit optimized workload, excluded from ordinary acceptance timing.
use super::{
    CURRENT, Fixture, MAX_GENERATION_BYTES, TaskPlanner, admission_with_planner, controls,
    destination_pass, owner, spec,
};
use crate::state::db::ReconciliationStateStore;
use crate::state::db::provider_generations::{
    pending_destination_reservations_for_path, take_reservation_read_counts,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[tokio::test]
#[ignore = "optimized synthetic measurement; run alone with one test thread"]
async fn v027_optimized_pending_lookup_and_normal_cycles() {
    for fallback in [false, true] {
        for count in [0, 100, 1000] {
            eprintln!(
                "workload mode={}",
                if fallback {
                    "cross-zone-fallback"
                } else {
                    "selected-ledger"
                }
            );
            let server = wiremock::MockServer::start().await;
            let bytes = include_bytes!("../../../../../../tests/data/media/pattern.jpg");
            Mock::given(method("GET"))
                .and(path("/one-copy"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
                .expect(1)
                .mount(&server)
                .await;
            let mut f = Fixture::new().await;
            f.config.folder_structure_albums = "same".into();
            f.config.concurrent_downloads = 1;
            f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/one-copy",server.uri()), "size":bytes.len(),"fileChecksum":CURRENT
            });
            let mut passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
            if fallback {
                for pass in &mut passes {
                    pass.album = pass
                        .album
                        .clone()
                        .with_cross_zone_sources(vec![f.pass.album.clone()]);
                }
            }
            let client = reqwest::Client::new();
            let first = crate::download::download_photos_with_sync(
                &client,
                &passes,
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(first.stats.downloaded, 1);
            let original = f.config.directory.join("same/changed.JPG");
            let mtime = std::fs::metadata(&original).unwrap().modified().unwrap();

            let setup = Instant::now();
            let (_, scope, zone) = f.pass.album.owned_private_scope().unwrap().unwrap();
            let mut pending_spec = spec();
            pending_spec.scope = scope;
            pending_spec.zone = zone.clone();
            pending_spec.profile["passes"][0]["scope"]["zone"] = zone;
            let root =
                f.db.begin_selection_generation(owner(), pending_spec, MAX_GENERATION_BYTES)
                    .await
                    .unwrap();
            let mut planner = TaskPlanner::for_download(None).await.unwrap();
            for index in 0..count {
                admission_with_planner(
                    &f.db,
                    &root.id,
                    &f.config.directory.join("unrelated-pending"),
                    index,
                    (false, false),
                    MAX_GENERATION_BYTES,
                    &mut planner,
                )
                .await;
            }
            eprintln!(
                "workload pending={count} setup_ms={}",
                setup.elapsed().as_millis()
            );

            let started = Instant::now();
            for _ in 0..10 {
                assert_eq!(
                    f.db.get_reconciliation_reservations().await.unwrap().len(),
                    count
                );
            }
            let full_us = started.elapsed().as_micros();
            let started = Instant::now();
            for _ in 0..10 {
                let conn = f.db.acquire_lock("paired path measurement").unwrap();
                assert!(
                    pending_destination_reservations_for_path(&conn, &original)
                        .unwrap()
                        .is_empty()
                );
            }
            eprintln!(
                "lookup pending={count} calls=10 full_us={full_us} path_us={}",
                started.elapsed().as_micros()
            );

            f.config.folder_structure_albums = "changed/{album}".into();
            take_reservation_read_counts();
            let started = Instant::now();
            let second = crate::download::download_photos_with_sync(
                &client,
                &passes,
                Arc::new(f.config.clone()),
                controls(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let elapsed = started.elapsed().as_micros();
            let reads = take_reservation_read_counts();
            assert_eq!(second.stats.downloaded, 0, "{second:?}");
            eprintln!(
                "normal baseline pending={count} selected_passes=2 elapsed_us={elapsed} full_reads={} path_reads={}",
                reads.0, reads.1
            );

            f.config.sync_mode = crate::download::SyncMode::Incremental {
                zone_sync_token: "quiet-measurement".into(),
            };
            let rank_before = f.session.rank_calls.load(Ordering::SeqCst);
            for cycle in 1..=2 {
                take_reservation_read_counts();
                let started = Instant::now();
                let quiet = crate::download::download_photos_with_sync(
                    &client,
                    &passes,
                    Arc::new(f.config.clone()),
                    controls(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
                let elapsed = started.elapsed().as_micros();
                let reads = take_reservation_read_counts();
                assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
                assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), rank_before);
                eprintln!(
                    "normal quiet pending={count} cycle={cycle} elapsed_us={elapsed} full_reads={} path_reads={}",
                    reads.0, reads.1
                );
            }
            assert_eq!(std::fs::read(&original).unwrap(), bytes);
            assert_eq!(
                std::fs::metadata(&original).unwrap().modified().unwrap(),
                mtime
            );
            assert!(!f.config.directory.join("changed").exists());
            let retained: i64 = f.db.acquire_lock("pending measurement conservation").unwrap().query_row(
            "SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND verified_media=0",
            [&root.id], |r|r.get(0)).unwrap();
            assert_eq!(retained as usize, count);
            server.verify().await;
        }
    }
}
