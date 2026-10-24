//! Opt-in live samples use disposable isolated storage; private values are never logged or exported.
use super::{DownloadConfig, DownloadControls};
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct ReadOnlyProbe(reqwest::Client, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl PhotosSession for ReadOnlyProbe {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let parsed = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid read URL"))?;
        anyhow::ensure!(
            [
                "/records/query",
                "/records/lookup",
                "/zones/list",
                "/changes/zone",
                "/internal/records/query/batch"
            ]
            .iter()
            .any(|suffix| parsed.path().ends_with(suffix)),
            "live validation refused a non-read endpoint"
        );
        anyhow::ensure!(
            self.1.fetch_add(1, Ordering::Relaxed) < 24,
            "sample read budget exhausted"
        );
        let bytes = crate::icloud::photos::session::diagnostic_bytes(
            &self.0,
            url,
            body,
            headers,
            8 * 1024 * 1024,
        )
        .await
        .map_err(|_| anyhow::anyhow!("private provider read failed"))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("private provider response invalid"))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
#[ignore = "requires explicit live-session and isolated-download authorization"]
async fn existing_session_primary_and_shared_samples_download_once_then_reuse_after_reopen()
-> Result<(), &'static str> {
    if std::env::var("KEI_SESSION_MEDIA_PROBE").as_deref() != Ok("1") {
        return Err("explicit isolated media opt-in missing");
    }
    let (session, service_root) =
        crate::auth::live_zone_cursor_test::existing_session_for_validation().await?;
    let probe = ReadOnlyProbe(session.http_client().clone(), Arc::new(AtomicUsize::new(0)));
    let mut samples = Vec::new();
    for database in ["private", "shared"] {
        let endpoint =
            format!("{service_root}/database/1/com.apple.photos.cloud/production/{database}");
        let listing = probe
            .post(
                &format!("{endpoint}/zones/list"),
                "{}".into(),
                &[("Content-type", "text/plain")],
            )
            .await
            .map_err(|_| "Photos discovery read failed")?;
        let zones = listing
            .get("zones")
            .and_then(Value::as_array)
            .ok_or("Photos zones missing")?;
        for zone in zones {
            if zone.get("deleted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let identity = zone.get("zoneID").ok_or("Photos scope missing")?;
            let name = identity
                .get("zoneName")
                .and_then(Value::as_str)
                .ok_or("Photos scope invalid")?;
            let category = if name == "PrimarySync" {
                "primary"
            } else if name.starts_with("SharedSync-") {
                "shared_photos"
            } else {
                continue;
            };
            if samples
                .iter()
                .any(|(existing, _, _, _): &(&str, String, String, Value)| *existing == category)
            {
                continue;
            }
            samples.push((
                category,
                database.to_owned(),
                endpoint.clone(),
                identity.clone(),
            ));
        }
    }
    if samples.len() != 2 {
        return Err("both Primary and Shared Photos scopes are required");
    }
    for (category, database, endpoint, identity) in samples {
        let params = Arc::new(std::collections::HashMap::from([
            ("remapEnums".to_owned(), Value::Bool(true)),
            ("getCurrentSyncToken".to_owned(), Value::Bool(true)),
        ]));
        let zone = Arc::new(identity);
        let library = PhotoLibrary::new(
            endpoint.clone(),
            Arc::clone(&params),
            Box::new(probe.clone()),
            Arc::clone(&zone),
            database,
            crate::retry::RetryConfig::default(),
        )
        .await
        .map_err(|_| "selected Photos library could not be initialized")?;
        // Small asset pages and a bounded request budget. The selected rendition
        // sizes advertised by the provider must total at most 32 MiB.
        let album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params,
                service_endpoint: endpoint.into(),
                name: "".into(),
                list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter: None,
                page_size: 3,
                zone_id: zone,
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(probe.clone()),
        );
        let candidates = album
            .photos(Some(3))
            .await
            .map_err(|_| "bounded Photos sample read failed")?;
        let asset = candidates
            .into_iter()
            .find(|asset| {
                !asset.versions().is_empty()
                    && asset
                        .versions()
                        .iter()
                        .try_fold(0_u64, |sum, (_, version)| sum.checked_add(version.size))
                        .is_some_and(|size| size > 0 && size <= 32 * 1024 * 1024)
            })
            .ok_or("no bounded downloadable media sample available")?;
        let isolated = tempfile::tempdir().map_err(|_| "isolated sample directory failed")?;
        let directory = isolated.path().join("media");
        let state = isolated.path().join("state.sqlite");
        let mut snapshot = Vec::new();
        let mut transferred = 0;
        for cycle in 0..3 {
            let db = Arc::new(
                SqliteStateDb::open(&state)
                    .await
                    .map_err(|_| "isolated sample state failed")?,
            );
            let config = Arc::new(DownloadConfig {
                directory: directory.clone().into(),
                folder_structure: "none".into(),
                library: library.zone_name().into(),
                state_db: Some(db.clone()),
                metadata: crate::config::MetadataConfig::default(),
                ..DownloadConfig::test_default()
            });
            let result = super::pipeline::stream_and_download_from_stream(
                session.download_client(),
                futures_util::stream::iter(vec![Ok::<_, anyhow::Error>(asset.clone())]),
                &config,
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                super::pipeline::StreamRuntime::new(None, None),
            )
            .await
            .map_err(|_| "isolated sample pipeline failed")?;
            if !result.failed.is_empty()
                || result.state_write_failures != 0
                || result.enumeration_errors != 0
            {
                return Err("isolated sample did not complete");
            }
            if cycle == 0 {
                if result.downloaded == 0 {
                    return Err("live sample did not download selected media");
                }
                transferred = result.downloaded;
                let rows = db
                    .get_downloaded_page(0, 100)
                    .await
                    .map_err(|_| "isolated sample receipt read failed")?;
                for row in rows {
                    let path = row.local_path.ok_or("live receipt has no path")?;
                    let sha = super::file::compute_sha256(&path)
                        .await
                        .map_err(|_| "live sample hash failed")?;
                    let mtime = std::fs::metadata(&path)
                        .and_then(|value| value.modified())
                        .map_err(|_| "live sample timestamp read failed")?;
                    snapshot.push((path, sha, mtime));
                }
            } else {
                if result.downloaded != 0
                    || result.bytes_downloaded != 0
                    || result.skip_summary.on_disk != transferred
                {
                    return Err("live sample was not reused completely after reopen");
                }
                for (path, sha, mtime) in &snapshot {
                    if super::file::compute_sha256(path)
                        .await
                        .map_err(|_| "reopened sample hash failed")?
                        != *sha
                        || std::fs::metadata(path)
                            .and_then(|value| value.modified())
                            .map_err(|_| "reopened timestamp read failed")?
                            != *mtime
                    {
                        return Err("existing live sample changed during reuse");
                    }
                }
            }
            drop(config);
            drop(db);
        }
        eprintln!(
            "scope={category} sample_assets=1 downloaded_renditions={transferred} reopened_reuse_cycles=2 existing_bytes_and_mtime_unchanged=true"
        );
    }
    eprintln!(
        "isolated_sample_only=true full_live_baseline_proven=false provider_writes=0 reauthentication_attempted=false"
    );
    Ok(())
}
