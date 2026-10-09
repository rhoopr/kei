#![cfg(test)]

use std::sync::Arc;

use bytes::Bytes;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{DownloadSingleContext, download_single_task};
use crate::download::file::{DownloadClient, DownloadResponse};
use crate::download::filter::{DownloadTask, MetadataPayload};
use crate::download::metadata_rewrite::MetadataFlags;
use crate::retry::RetryConfig;
use crate::state::{MediaType, VersionSizeKey};

type ClientError = Box<dyn std::error::Error + Send + Sync>;

struct StaticClient {
    body: Vec<u8>,
}

#[async_trait::async_trait]
impl DownloadClient for StaticClient {
    async fn fetch(&self, _: &str, _: Option<u64>) -> Result<DownloadResponse, ClientError> {
        let body = Bytes::copy_from_slice(&self.body);
        Ok(DownloadResponse {
            status: 200,
            content_length: Some(body.len() as u64),
            content_range: None,
            content_type: Some("image/jpeg".into()),
            stream: Box::pin(futures_util::stream::once(async move { Ok(body) })),
        })
    }
}

struct PausedClient {
    body: Vec<u8>,
    written: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl DownloadClient for PausedClient {
    async fn fetch(&self, _: &str, resume: Option<u64>) -> Result<DownloadResponse, ClientError> {
        let body = self.body.clone();
        let offset = resume.unwrap_or(0) as usize;
        let written = self.written.clone();
        let release = self.release.clone();
        Ok(DownloadResponse {
            status: if resume.is_some() { 206 } else { 200 },
            content_length: Some((body.len() - offset) as u64),
            content_range: resume
                .map(|start| format!("bytes {start}-{}/{}", body.len() - 1, body.len())),
            content_type: Some("image/jpeg".into()),
            stream: Box::pin(futures_util::stream::unfold(0, move |step| {
                let body = body.clone();
                let written = written.clone();
                let release = release.clone();
                async move {
                    match step {
                        0 => Some((Ok(Bytes::copy_from_slice(&body[offset..offset + 32])), 1)),
                        1 => {
                            written.notify_one();
                            release.notified().await;
                            Some((Ok(Bytes::copy_from_slice(&body[offset + 32..])), 2))
                        }
                        _ => None,
                    }
                }
            })),
        })
    }
}

fn task(path: std::path::PathBuf, id: &str, size: u64) -> DownloadTask {
    DownloadTask {
        url: "synthetic".into(),
        download_path: path,
        replacement_fingerprint: None,
        pending_cross_parent_root: None,
        checksum: "AAAA".into(),
        asset_id: id.into(),
        asset_record_name: id.into(),
        library: "PrimarySync".into(),
        metadata: Arc::new(MetadataPayload::default()),
        size,
        created_local: chrono::Local::now().fixed_offset(),
        version_size: VersionSizeKey::Original,
        media_type: MediaType::Photo,
    }
}

async fn wait_for_part(path: &std::path::Path, length: u64) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while tokio::fs::metadata(path).await.unwrap().len() != length {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

async fn run<C: DownloadClient>(
    client: &C,
    task: &DownloadTask,
    token: &CancellationToken,
    db: Option<&dyn crate::download::DownloadStore>,
) -> anyhow::Result<super::DownloadSingleResult> {
    download_single_task(
        client,
        task,
        &RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        MetadataFlags::default(),
        DownloadSingleContext {
            temp_suffix: ".kei-tmp",
            state_db: db,
            rate_limit_counter: None,
            bandwidth_limiter: None,
            shutdown_token: token,
            mode: crate::personality::Mode::Off,
        },
    )
    .await
}

#[tokio::test]
async fn issue_770_recovery_fresh_task_transports_original_publication_owner() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("old-album/pending.jpg");
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let client = StaticClient { body: body.clone() };
    let mut pending = task(path.clone(), "PENDING", body.len() as u64);
    pending.pending_cross_parent_root = Some(Arc::new(dir.path().to_path_buf()));
    let result = run(&client, &pending, &CancellationToken::new(), None)
        .await
        .unwrap();
    let proof = result
        .5
        .expect("affected transfer must transport its original publication proof");
    assert_eq!(
        data_encoding::HEXLOWER.encode(&proof.fingerprint.sha256),
        result.1
    );
    proof.validate().await.unwrap();
    std::fs::rename(&path, path.with_extension("preserved")).unwrap();
    std::fs::write(&path, &body).unwrap();
    assert!(
        proof.validate().await.is_err(),
        "same bytes on another inode are not the publication owner"
    );
    assert_eq!(
        std::fs::read(path.with_extension("preserved")).unwrap(),
        body
    );
}

#[tokio::test]
async fn byte_identical_twins_interleaved_stream_have_independent_staging() {
    let dir = TempDir::new().unwrap();
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let a = task(dir.path().join("a.jpg"), "A", body.len() as u64);
    let b = task(dir.path().join("b.jpg"), "B", body.len() as u64);
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first = PausedClient {
        body: body.clone(),
        written: written.clone(),
        release: release.clone(),
    };
    let first_task = a.clone();
    let first_handle =
        tokio::spawn(
            async move { run(&first, &first_task, &CancellationToken::new(), None).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(5), written.notified())
        .await
        .unwrap();
    let second = StaticClient { body: body.clone() };
    let second_result = run(&second, &b, &CancellationToken::new(), None).await;
    release.notify_one();
    let first_result = first_handle.await.unwrap();
    assert!(second_result.is_ok(), "second: {second_result:?}");
    assert!(first_result.is_ok(), "first: {first_result:?}");
    assert_eq!(std::fs::read(a.download_path).unwrap(), body);
    assert_eq!(std::fs::read(b.download_path).unwrap(), body);
}

#[tokio::test]
async fn resumed_owner_survives_same_identity_and_cross_version_waiter_cancellation() {
    for changed_generation in [false, true] {
        let dir = TempDir::new().unwrap();
        let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
        let a = task(dir.path().join("a.jpg"), "A", body.len() as u64);
        let part =
            crate::download::file::temp_download_path(&a.download_path, &a.checksum, ".kei-tmp")
                .unwrap();
        std::fs::write(&part, &body[..32]).unwrap();
        let db_path = dir.path().join("state.db");
        {
            let db = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
            db.claim_temp_file(&part).await.unwrap();
        }
        let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let written = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let first = PausedClient {
            body: body.clone(),
            written: written.clone(),
            release: release.clone(),
        };
        let first_task = a.clone();
        let first_db = db.clone();
        let owner = tokio::spawn(async move {
            run(
                &first,
                &first_task,
                &CancellationToken::new(),
                Some(first_db.as_ref()),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), written.notified())
            .await
            .unwrap();
        wait_for_part(&part, 64).await;
        assert_eq!(
            std::fs::read(&part).unwrap(),
            body[..64],
            "must resume the retained prefix"
        );
        let mut duplicate = a.clone();
        if changed_generation {
            duplicate.checksum = "AAAB".into();
        }
        let client = StaticClient { body: body.clone() };
        let token = CancellationToken::new();
        let waiter = run(&client, &duplicate, &token, Some(db.as_ref()));
        tokio::pin!(waiter);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), waiter.as_mut())
                .await
                .is_err(),
            "waiter must not reach HTTP or mutate peer staging"
        );
        token.cancel();
        let error = waiter.await.unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::download::error::DownloadError>()
                .unwrap()
                .is_interrupted()
        );
        assert_eq!(std::fs::read(&part).unwrap(), body[..64]);
        let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
        let claims = reopened
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].path, part);
        release.notify_one();
        assert!(owner.await.unwrap().is_ok());
        assert_eq!(std::fs::read(&a.download_path).unwrap(), body);
        assert!(!part.exists());
        assert!(
            reopened
                .get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        // The lease's stable inode survives restart, but its ownership does not.
        assert!(
            run(&client, &a, &CancellationToken::new(), Some(&reopened))
                .await
                .is_ok()
        );
        assert_eq!(std::fs::read(a.download_path).unwrap(), body);
    }
}

#[tokio::test]
async fn failed_claim_and_failed_peer_retirement_preserve_streaming_owner_evidence() {
    let dir = TempDir::new().unwrap();
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let a = task(dir.path().join("a.jpg"), "A", body.len() as u64);
    let b = task(dir.path().join("b.jpg"), "B", body.len() as u64);
    let part = crate::download::file::temp_download_path(&a.download_path, &a.checksum, ".kei-tmp")
        .unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first = PausedClient {
        body: body.clone(),
        written: written.clone(),
        release: release.clone(),
    };
    let first_task = a.clone();
    let first_db = db.clone();
    let owner = tokio::spawn(async move {
        run(
            &first,
            &first_task,
            &CancellationToken::new(),
            Some(first_db.as_ref()),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), written.notified())
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("fail claim").unwrap();
        conn.execute_batch("CREATE TRIGGER fail_claim BEFORE INSERT ON owned_temp_files BEGIN SELECT RAISE(ABORT, 'synthetic claim failure'); END;").unwrap();
    }
    let client = StaticClient { body: body.clone() };
    assert!(
        run(&client, &b, &CancellationToken::new(), Some(db.as_ref()))
            .await
            .unwrap_err()
            .to_string()
            .contains("ownership")
    );
    {
        let conn = db.acquire_lock("restore claim").unwrap();
        conn.execute_batch("DROP TRIGGER fail_claim").unwrap();
    }
    // An invalid peer transfer reaches the ordinary failure/retirement branch.
    let bad = StaticClient {
        body: b"not media".to_vec(),
    };
    assert!(
        run(&bad, &b, &CancellationToken::new(), Some(db.as_ref()))
            .await
            .is_err()
    );
    let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
    let claims = reopened
        .get_owned_temp_files_before(i64::MAX)
        .await
        .unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].path, part);
    assert_eq!(std::fs::read(&part).unwrap(), body[..32]);
    release.notify_one();
    assert!(owner.await.unwrap().is_ok());
    assert_eq!(std::fs::read(a.download_path).unwrap(), body);
    assert!(!b.download_path.exists());
    assert!(
        reopened
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn changed_generation_waits_then_preserves_no_overwrite_publication() {
    let dir = TempDir::new().unwrap();
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let a = task(dir.path().join("a.jpg"), "A", body.len() as u64);
    let mut b = a.clone();
    b.checksum = "AAAB".into();
    let old_part =
        crate::download::file::temp_download_path(&a.download_path, &a.checksum, ".kei-tmp")
            .unwrap();
    let new_part =
        crate::download::file::temp_download_path(&b.download_path, &b.checksum, ".kei-tmp")
            .unwrap();
    assert_ne!(old_part, new_part);
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first = PausedClient {
        body: body.clone(),
        written: written.clone(),
        release: release.clone(),
    };
    let first_task = a.clone();
    let owner =
        tokio::spawn(
            async move { run(&first, &first_task, &CancellationToken::new(), None).await },
        );
    written.notified().await;
    let mut changed = body.clone();
    *changed.last_mut().unwrap() ^= 1;
    let client = StaticClient {
        body: changed.clone(),
    };
    let token = CancellationToken::new();
    let waiter = run(&client, &b, &token, None);
    tokio::pin!(waiter);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), waiter.as_mut())
            .await
            .is_err()
    );
    assert!(!new_part.exists());
    release.notify_one();
    assert!(owner.await.unwrap().is_ok());
    assert!(
        waiter.await.is_err(),
        "different bytes cannot overwrite an earlier generation"
    );
    assert_eq!(std::fs::read(a.download_path).unwrap(), body);
    assert_eq!(std::fs::read(new_part).unwrap(), changed);
}

#[tokio::test]
async fn twins_finish_both_passes_with_independent_metadata_and_reopened_state() {
    use crate::download::pipeline::pass::{PassConfig, run_download_pass};
    use crate::download::pipeline::streaming::{StreamRuntime, stream_and_download_from_stream};
    use crate::download::planner::{TaskPlanner, upsert_seen_for_task};
    use crate::download::{DownloadConfig, DownloadControls, DownloadReporting};
    use crate::test_helpers::TestPhotoAsset;
    use base64::Engine;
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    #[cfg(feature = "xmp")]
    let modes = [(false, false), (true, false), (false, true), (true, true)];
    #[cfg(not(feature = "xmp"))]
    let modes = [(false, false), (true, false)];
    for (explicit, embed) in modes {
        let dir = TempDir::new().unwrap();
        let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
        // MMCS generation is deliberately unrelated to the local content hash.
        let checksum = base64::engine::general_purpose::STANDARD.encode([0x72; 21]);
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(50))
                    .set_body_bytes(body.clone()),
            )
            .expect(2)
            .mount(&server)
            .await;
        let db_path = dir.path().join("state.db");
        let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.state_db = Some(db.clone());
        config.retry.max_retries = 0;
        config.temp_suffix = Arc::from(".custom-part");
        #[cfg(feature = "xmp")]
        {
            config.metadata = crate::config::MetadataConfig {
                xmp_sidecar: true,
                embed_xmp: embed,
                set_exif_rating: embed,
                ..Default::default()
            };
        }
        let config = Arc::new(config);
        let assets: Vec<_> = ["TWIN-A", "TWIN-B"]
            .into_iter()
            .map(|id| {
                TestPhotoAsset::new(id)
                    .filename(&format!("{id}.jpg"))
                    .orig_size(body.len() as u64)
                    .orig_checksum(&checksum)
                    .favorite(id == "TWIN-B")
                    .orig_url(&format!("{}/twins.jpg", server.uri()))
                    .build()
            })
            .collect();
        let client = reqwest::Client::new();
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        let mut tasks = Vec::new();
        for asset in &assets {
            let plan = planner.plan_download_asset(asset, &config).await.unwrap();
            assert_eq!(plan.tasks.len(), 1);
            for task in plan.tasks {
                upsert_seen_for_task(db.as_ref(), &config, asset, &task)
                    .await
                    .unwrap();
                tasks.push(task);
            }
        }
        assert_ne!(tasks[0].download_path, tasks[1].download_path);
        if explicit {
            let result = run_download_pass(
                PassConfig {
                    prior_auth_errors: 0,
                    url_obtained_at: Default::default(),
                    client: &client,
                    retry_config: &config.retry,
                    metadata: MetadataFlags::from(&config.metadata),
                    mark_capture_repair_after_download: false,
                    concurrency: 2,
                    reporting: DownloadReporting::hidden(),
                    temp_suffix: config.temp_suffix.clone(),
                    shutdown_token: CancellationToken::new(),
                    state_db: config.state_db.clone(),
                    rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    bandwidth_limiter: None,
                    library: config.library.clone(),
                },
                tasks.clone(),
            )
            .await;
            assert_eq!(result.downloaded, 2);
            assert!(result.failed.is_empty());
            assert_eq!(result.exif_failures, 0);
        } else {
            let result = stream_and_download_from_stream(
                &client,
                futures_util::stream::iter(assets.clone().into_iter().map(Ok)),
                &config,
                DownloadControls::download_hidden(),
                2,
                CancellationToken::new(),
                StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(result.downloaded, 2);
            assert!(result.failed.is_empty());
            assert_eq!(result.exif_failures, 0);
        }
        for task in &tasks {
            let bytes = std::fs::read(&task.download_path).unwrap();
            if embed && task.metadata.rating.is_some() {
                assert_ne!(
                    bytes, body,
                    "embed must exercise replacement before publication"
                );
            } else {
                assert_eq!(bytes, body);
            }
            assert!(
                !crate::download::file::temp_download_path(
                    &task.download_path,
                    &checksum,
                    &config.temp_suffix
                )
                .unwrap()
                .exists()
            );
            #[cfg(feature = "xmp")]
            {
                let sidecar = std::fs::read_to_string(
                    crate::download::pipeline::test_support::sidecar_path_for(&task.download_path),
                )
                .unwrap();
                let xmp: xmp_toolkit::XmpMeta = sidecar.parse().unwrap();
                assert_eq!(
                    xmp.property(xmp_toolkit::xmp_ns::XMP, "Rating")
                        .map(|value| value.value),
                    (task.asset_id.as_ref() == "TWIN-B").then(|| "5".to_string()),
                    "metadata identity must belong to this twin"
                );
            }
        }
        let reopened = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(reopened.get_failed().await.unwrap().is_empty());
        assert!(
            reopened
                .get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
        use sha2::{Digest, Sha256};
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&body));
        for row in &rows {
            let bytes = std::fs::read(row.local_path.as_ref().unwrap()).unwrap();
            let local_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes));
            assert_eq!(row.local_checksum.as_deref(), Some(local_hash.as_str()));
            assert_eq!(row.download_checksum.as_deref(), Some(hash.as_str()));
        }
        let mut next_config = (*config).clone();
        next_config.state_db = Some(reopened);
        let unchanged = stream_and_download_from_stream(
            &client,
            futures_util::stream::iter(assets.into_iter().map(Ok)),
            &Arc::new(next_config),
            DownloadControls::download_hidden(),
            2,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(unchanged.downloaded, 0);
        assert!(unchanged.failed.is_empty());
        server.verify().await;
    }
}

#[tokio::test]
async fn cancelled_twin_retains_only_its_prefix_and_resumes_after_reopen() {
    let dir = TempDir::new().unwrap();
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let a = task(dir.path().join("a.jpg"), "A", body.len() as u64);
    let b = task(dir.path().join("b.jpg"), "B", body.len() as u64);
    let part = crate::download::file::temp_download_path(&a.download_path, &a.checksum, ".kei-tmp")
        .unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(crate::state::SqliteStateDb::open(&db_path).await.unwrap());
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let token = CancellationToken::new();
    let first = PausedClient {
        body: body.clone(),
        written: written.clone(),
        release,
    };
    let first_task = a.clone();
    let first_db = db.clone();
    let first_token = token.clone();
    let owner = tokio::spawn(async move {
        run(&first, &first_task, &first_token, Some(first_db.as_ref())).await
    });
    written.notified().await;
    let client = StaticClient { body: body.clone() };
    assert!(
        run(&client, &b, &CancellationToken::new(), Some(db.as_ref()))
            .await
            .is_ok()
    );
    token.cancel();
    assert!(
        owner
            .await
            .unwrap()
            .unwrap_err()
            .downcast_ref::<crate::download::error::DownloadError>()
            .unwrap()
            .is_interrupted()
    );
    assert_eq!(std::fs::read(&part).unwrap(), body[..32]);
    assert_eq!(std::fs::read(&b.download_path).unwrap(), body);
    drop(db);
    let reopened = crate::state::SqliteStateDb::open(&db_path).await.unwrap();
    assert!(
        reopened
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    release.notify_one();
    let resumed = PausedClient {
        body: body.clone(),
        written,
        release,
    };
    assert!(
        run(&resumed, &a, &CancellationToken::new(), Some(&reopened))
            .await
            .is_ok()
    );
    assert_eq!(std::fs::read(a.download_path).unwrap(), body);
    assert_eq!(std::fs::read(b.download_path).unwrap(), body);
    assert!(!part.exists());
    assert!(
        reopened
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn legacy_checksum_staging_is_neither_adopted_nor_retired_by_new_task() {
    let dir = TempDir::new().unwrap();
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let a = task(dir.path().join("a.jpg"), "A", body.len() as u64);
    let legacy = dir.path().join("AAAAA.kei-tmp");
    std::fs::write(&legacy, b"ambiguous legacy evidence").unwrap();
    let db = crate::state::SqliteStateDb::open(&dir.path().join("state.db"))
        .await
        .unwrap();
    db.claim_temp_file(&legacy).await.unwrap();
    let client = StaticClient { body: body.clone() };
    assert!(
        run(&client, &a, &CancellationToken::new(), Some(&db))
            .await
            .is_ok()
    );
    assert_eq!(std::fs::read(a.download_path).unwrap(), body);
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        b"ambiguous legacy evidence"
    );
    let claims = db.get_owned_temp_files_before(i64::MAX).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].path, legacy);
}

#[tokio::test]
async fn streaming_auth_abort_drains_peer_claim_and_lease() {
    use crate::download::DownloadConfig;
    use crate::download::pipeline::StreamPipelineShared;
    use crate::download::pipeline::consumer::{
        StreamConsumerSettings, consume_stream_download_tasks,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let dir = TempDir::new().unwrap();
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let server = wiremock::MockServer::start().await;
    let requested = Arc::new(Notify::new());
    let signal = requested.clone();
    Mock::given(method("GET"))
        .and(path("/active"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(5))
                .set_body_bytes(body.clone())
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(401).set_delay(std::time::Duration::from_millis(200)))
        .expect(3)
        .mount(&server)
        .await;
    let db = Arc::new(
        crate::state::SqliteStateDb::open(&dir.path().join("state.db"))
            .await
            .unwrap(),
    );
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    for index in 0..4 {
        let mut task = task(
            dir.path().join(format!("{index}.jpg")),
            &format!("TASK-{index}"),
            0,
        );
        task.url = format!(
            "{}/{}",
            server.uri(),
            if index == 0 { "active" } else { "auth" }
        )
        .into();
        db.upsert_seen(&crate::state::AssetRecord::new_pending(
            "PrimarySync".into(),
            task.asset_id.to_string(),
            task.version_size,
            task.checksum.to_string(),
            task.download_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            chrono::Utc::now(),
            None,
            0,
            MediaType::Photo,
        ))
        .await
        .unwrap();
        tx.send(task).await.unwrap();
    }
    drop(tx);
    let shutdown = CancellationToken::new();
    let pipeline_shutdown = shutdown.clone();
    let worker = tokio::spawn(async move {
        consume_stream_download_tasks(
            rx,
            reqwest::Client::new(),
            StreamPipelineShared {
                config: config.clone(),
                state_db: config.state_db.clone(),
                pb: indicatif::ProgressBar::hidden(),
                pipeline_shutdown,
            },
            StreamConsumerSettings {
                retry_config: RetryConfig {
                    max_retries: 0,
                    ..Default::default()
                },
                metadata_flags: MetadataFlags::default(),
                concurrency: 4,
                mode: crate::personality::Mode::Off,
                bytes_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), requested.notified())
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.auth_errors, 3);
    assert!(
        shutdown.is_cancelled(),
        "auth abort must cancel and drain peers"
    );
    assert!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty(),
        "drain must retire peer claims before lease release"
    );
    let destination = dir.path().join("0.jpg");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::download::file::lock_download_destination(
                &destination,
                &CancellationToken::new()
            )
        )
        .await
        .unwrap()
        .is_ok()
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn auth_abort_waits_for_detached_metadata_publication_before_unlock() {
    use crate::download::DownloadConfig;
    use crate::download::pipeline::StreamPipelineShared;
    use crate::download::pipeline::consumer::{
        StreamConsumerSettings, consume_stream_download_tasks,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let dir = TempDir::new().unwrap();
    let body = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
    let server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/active"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth"))
        .respond_with(ResponseTemplate::new(401))
        .expect(3)
        .mount(&server)
        .await;
    let db = Arc::new(
        crate::state::SqliteStateDb::open(&dir.path().join("state.db"))
            .await
            .unwrap(),
    );
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);
    let destination = dir.path().join("0.jpg");
    let part = crate::download::file::temp_download_path(&destination, "AAAA", &config.temp_suffix)
        .unwrap();
    let pause = crate::download::metadata_rewrite::publication_pause::install(&part);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let mut auth_tasks = Vec::new();
    for index in 0..4 {
        let mut task = task(
            dir.path().join(format!("{index}.jpg")),
            &format!("TASK-{index}"),
            body.len() as u64,
        );
        task.url = format!(
            "{}/{}",
            server.uri(),
            if index == 0 { "active" } else { "auth" }
        )
        .into();
        if index == 0 {
            task.metadata = Arc::new(MetadataPayload {
                rating: Some(5),
                ..Default::default()
            });
        }
        db.upsert_seen(&crate::state::AssetRecord::new_pending(
            "PrimarySync".into(),
            task.asset_id.to_string(),
            task.version_size,
            task.checksum.to_string(),
            task.download_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            chrono::Utc::now(),
            None,
            body.len() as u64,
            MediaType::Photo,
        ))
        .await
        .unwrap();
        if index == 0 {
            tx.send(task).await.unwrap();
        } else {
            auth_tasks.push(task);
        }
    }
    let shutdown = CancellationToken::new();
    let pipeline_shutdown = shutdown.clone();
    let mut worker = tokio::spawn(async move {
        consume_stream_download_tasks(
            rx,
            reqwest::Client::new(),
            StreamPipelineShared {
                config: config.clone(),
                state_db: config.state_db.clone(),
                pb: indicatif::ProgressBar::hidden(),
                pipeline_shutdown,
            },
            StreamConsumerSettings {
                retry_config: RetryConfig {
                    max_retries: 0,
                    ..Default::default()
                },
                metadata_flags: MetadataFlags::RATING,
                concurrency: 4,
                mode: crate::personality::Mode::Off,
                bytes_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.started())
        .await
        .unwrap();
    assert!(!shutdown.is_cancelled());
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/auth")
            .count(),
        0,
        "auth failures must not start before publication is paused"
    );
    // Introduce auth failures only after detached publication owns the destination.
    for task in auth_tasks {
        tx.send(task).await.unwrap();
    }
    drop(tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.cancelled())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut worker)
            .await
            .is_err(),
        "consumer must await detached publication"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            crate::download::file::lock_download_destination(
                &destination,
                &CancellationToken::new()
            )
        )
        .await
        .is_err(),
        "peer cannot acquire before detached publication completes"
    );
    assert_eq!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.get_failed().await.unwrap().len(),
        3,
        "third auth error must finalize before drain completes"
    );
    pause.release();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.auth_errors, 3);
    assert_eq!(result.downloaded, 1);
    assert_eq!(result.failed.len(), 3);
    assert_ne!(std::fs::read(&destination).unwrap(), body);
    assert!(
        db.get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), 1);
    assert!(
        crate::download::file::lock_download_destination(&destination, &CancellationToken::new())
            .await
            .is_ok()
    );
}
