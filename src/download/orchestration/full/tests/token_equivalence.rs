//! Synthetic provider -> full sync -> persistent history -> offline export.
use super::{
    AlbumPass, DownloadControls, PassKind, PhotosSession, album_with_session,
    download_photos_full_with_token, test_config,
};
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Clone)]
struct TokenSession {
    queries: Arc<AtomicUsize>,
    moving: bool,
    second_group: bool,
}
#[async_trait::async_trait]
impl PhotosSession for TokenSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({"batch":[{"records":[{"fields":{"itemCount":{"value":0}}}]}]}));
        }
        assert_eq!(
            serde_json::from_str::<Value>(&body)?["zoneID"]["zoneName"],
            "PrimarySync"
        );
        let clock = self.queries.fetch_add(1, Ordering::SeqCst);
        // Five empty page/probe requests per pass. A single virtual zone clock
        // changes twice per round, with no query/scope split in the moving case.
        let group = if self.moving {
            clock / 115
        } else {
            usize::from(self.second_group)
        };
        Ok(json!({"records":[], "syncToken":format!("PRIVATE_TOKEN_{group}")}))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn token_equivalence_46_pass_persistent_and_moving_export_roundtrip() {
    for moving in [false, true] {
        let queries = Arc::new(AtomicUsize::new(0));
        let passes = (0..46)
            .map(|index| AlbumPass {
                kind: PassKind::Album,
                album: album_with_session(
                    "PrimarySync",
                    &format!("PRIVATE_ALBUM_{index}"),
                    Box::new(TokenSession {
                        queries: Arc::clone(&queries),
                        moving,
                        second_group: index >= 23,
                    }),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            })
            .collect::<Vec<_>>();
        let root = TempDir::new().unwrap();
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let username = "private-user@example.invalid";
        let history = crate::support::history_path(root.path(), username, "com");
        let (recorder, guard) = crate::support::history::start(history.clone(), json!({})).unwrap();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder));
        let mut config = test_config();
        config.directory = Arc::from(media.as_path());
        config.concurrent_downloads = 10;
        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.sync_token, None);
        assert_eq!(result.checkpoint.retry_passes.len(), 46);
        assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
        assert_eq!(result.stats.same_cycle_recovery_successes, 0);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(queries.load(Ordering::SeqCst), 460);
        guard.finish().await;
        let before = std::fs::read(&history).unwrap();
        let config_path = root.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "data_dir={}\n[auth]\nusername={}\n",
                json!(root.path().to_str().unwrap()),
                json!(username)
            ),
        )
        .unwrap();
        let output = root.path().join("export.json");
        crate::support::export(
            crate::cli::SupportExportArgs {
                output: output.clone(),
            },
            config_path.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&history).unwrap(), before);
        let text = std::fs::read_to_string(output).unwrap();
        for secret in [
            "PRIVATE_",
            "PrimarySync",
            username,
            root.path().to_str().unwrap(),
        ] {
            assert!(!text.contains(secret), "leaked {secret}");
        }
        let bundle: Value = serde_json::from_str(&text).unwrap();
        let events = bundle["history"]["data"]["cycles"][0]["diagnostics"]
            .as_array()
            .unwrap();
        let evidence = &events
            .iter()
            .find(|event| {
                event["kind"] == "support_token_equivalence_v1"
                    && event["fields"]["phase"] == "same_cycle_recovery"
            })
            .expect("final evidence exported")["fields"];
        assert_eq!(evidence["passes_total"], 46);
        assert_eq!(evidence["passes_omitted"], 0);
        assert_eq!(evidence["rows_rejected"], 0);
        let rows = evidence["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 46);
        let initial = rows
            .iter()
            .map(|r| r["initial_group"].as_u64().unwrap())
            .collect::<FxHashSet<_>>();
        let recovered = rows
            .iter()
            .map(|r| r["recovery_group"].as_u64().unwrap())
            .collect::<FxHashSet<_>>();
        assert_eq!(initial.len(), 2);
        assert_eq!(recovered.len(), 2);
        assert_eq!(initial.is_disjoint(&recovered), moving);
        for row in rows {
            assert_eq!(row["scope"], "same_first_pass");
            assert_eq!(row["initial_status"], "present");
            assert_eq!(row["recovery_status"], "present");
            if !moving {
                assert_eq!(row["initial_group"], row["recovery_group"]);
            }
        }
        for key in ["initial_completion_order", "recovery_completion_order"] {
            assert_eq!(
                rows.iter()
                    .map(|r| r[key].as_u64().unwrap())
                    .collect::<FxHashSet<_>>(),
                (1..=46).collect()
            );
        }
        assert_eq!(std::fs::read_dir(media).unwrap().count(), 0);
    }
}
