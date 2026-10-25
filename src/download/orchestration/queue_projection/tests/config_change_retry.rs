use super::{Fixture, controls, destination_pass, destination_pass_with, owner};
use crate::download::DownloadStore;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::SqliteStateDb;
use crate::state::db::provider_selection::SelectionPath;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Change {
    AddAlbum,
    AllowedFilter,
    ExcludedThenRestored,
    RemovedThenRestored,
}

#[tokio::test]
async fn v027_added_album_preserves_quiet_reopened_retry() {
    config_change_retry(Change::AddAlbum).await;
}

#[tokio::test]
async fn v027_changed_filename_filter_preserves_eligible_quiet_retry() {
    config_change_retry(Change::AllowedFilter).await;
}

#[tokio::test]
async fn v027_excluded_pending_copy_defers_then_retries_when_filter_restored() {
    config_change_retry(Change::ExcludedThenRestored).await;
}

#[tokio::test]
async fn v027_removed_album_defers_pending_copy_until_selected_again() {
    config_change_retry(Change::RemovedThenRestored).await;
}

#[test]
fn v027_retry_compatibility_rejects_missing_malformed_and_different_keys() {
    use crate::state::db::provider_generations::retained_profiles_compatible as compatible;
    let old = json!({"passes":["A"],"admitted_retry_policy":"a".repeat(64)});
    let new = json!({"passes":["A","B"],"admitted_retry_policy":"a".repeat(64)});
    assert!(compatible(&old, &new));
    for key in [
        json!(null),
        json!(""),
        json!("a".repeat(63)),
        json!("G".repeat(64)),
        json!("b".repeat(64)),
    ] {
        assert!(!compatible(
            &old,
            &json!({"passes":["A","B"],"admitted_retry_policy":key})
        ));
    }
    assert!(!compatible(
        &json!({"passes":["A"]}),
        &json!({"passes":["B"]})
    ));
}

async fn config_change_retry(change: Change) {
    use crate::state::DownloadStateStore;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::AtomicBool;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/pattern.jpg"
    ));
    let ready = Arc::new(AtomicBool::new(false));
    let gets = Arc::new(AtomicUsize::new(0));
    let available = ready.clone();
    let attempts = gets.clone();
    Mock::given(method("GET"))
        .and(path("/copy"))
        .respond_with(move |_request: &wiremock::Request| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 || available.load(Ordering::SeqCst) {
                ResponseTemplate::new(200).set_body_bytes(bytes.as_slice())
            } else {
                ResponseTemplate::new(500)
            }
        })
        .mount(&server)
        .await;
    let mut f = Fixture::new().await;
    f.config.folder_structure_albums = "{album}".into();
    f.config.concurrent_downloads = 1;
    f.config.retry.max_retries = 0;
    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({
        "downloadURL":format!("{}/copy",server.uri()), "size":bytes.len(),
        "fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(bytes))
    });
    let passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    let first = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.stats.downloaded, 1, "{first:?}");
    let a = f.config.directory.join("A/changed.JPG");
    let b = f.config.directory.join("B/changed.JPG");
    assert_eq!(std::fs::read(&a).unwrap(), bytes);
    assert!(!b.exists());
    let a_mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
    let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
    // Model A's verified completion arriving after B's failure. This uses the
    // real receipt owner and leaves B's independently admitted destination pending.
    f.db.mark_verified_download(
        "PrimarySync",
        "asset-m",
        "original",
        &a,
        &digest,
        Some(&digest),
        false,
    )
    .await
    .unwrap();
    assert!(f.db.get_failed().await.unwrap().is_empty());
    assert!(f.db.get_pending().await.unwrap().is_empty());
    let original_b: (String,String,String) = f.db.acquire_lock("read pending physical copy").unwrap()
        .query_row("SELECT generation,pass_key,path FROM provider_active_destinations WHERE child='asset-m' AND admitted=1 AND verified_media=0",
            [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        serde_json::from_str::<SelectionPath>(&original_b.2)
            .unwrap()
            .to_path(),
        b
    );
    f.db.make_selection_retry_due(owner(), "asset-m".into())
        .await
        .unwrap();
    drop(passes);
    f.config.state_db = None;
    drop(f.capture);
    drop(f.pass);
    drop(f.db);
    let reopened = Arc::new(
        SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    f.db = reopened;
    f.capture = ShadowCapture::new(f.db.clone(), owner(), "com");
    f.config.state_db = Some(f.db.clone() as Arc<dyn DownloadStore>);
    f.pass = destination_pass_with(&f.session, &f.capture, "A");
    f.config.folder_structure_albums = "changed-template/{album}".into();
    f.config.sync_mode = crate::download::SyncMode::Incremental {
        zone_sync_token: "quiet-before-drift".into(),
    };
    ready.store(true, Ordering::SeqCst);
    let before_gets = gets.load(Ordering::SeqCst);
    let before_ranks = f.session.rank_calls.load(Ordering::SeqCst);
    let mut passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    match change {
        Change::AddAlbum => passes.push(destination_pass(&f, "C")),
        Change::AllowedFilter => {
            f.config.filename_exclude = Arc::from([glob::Pattern::new("*.MOV").unwrap()]);
            f.config.enum_config_hash = Some("changed-filename-policy".into());
        }
        Change::ExcludedThenRestored => {
            f.config.filename_exclude = Arc::from([glob::Pattern::new("*.JPG").unwrap()]);
            f.config.enum_config_hash = Some("excluded-filename-policy".into());
        }
        Change::RemovedThenRestored => {
            passes.pop();
        }
    }
    if matches!(
        change,
        Change::ExcludedThenRestored | Change::RemovedThenRestored
    ) {
        let deferred = crate::download::download_photos_with_sync(
            &reqwest::Client::new(),
            &passes,
            Arc::new(f.config.clone()),
            controls(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(deferred.stats.downloaded, 0, "{deferred:?}");
        assert_eq!(gets.load(Ordering::SeqCst), before_gets);
        assert!(!b.exists());
        let pending: bool = f.db.acquire_lock("retained owner not excluded").unwrap()
            .query_row("SELECT w.admitted=1 AND w.verified_media=0 AND d.admission='admitted' AND d.next_retry_at>0 FROM provider_active_destinations w JOIN provider_active_decisions d ON d.generation=w.generation AND d.pass_key=w.pass_key AND d.child=w.child WHERE w.generation=?1 AND w.pass_key=?2 AND w.child='asset-m' AND w.path=?3",
                rusqlite::params![original_b.0,original_b.1,original_b.2],|r|r.get(0)).unwrap();
        assert!(
            pending,
            "removed/filtered selection must preserve its admitted owner"
        );
        f.db.make_selection_retry_due(owner(), "asset-m".into())
            .await
            .unwrap();
        f.config.filename_exclude = Arc::from([]);
        f.config.enum_config_hash = Some("restored-policy".into());
        passes = vec![destination_pass(&f, "A"), destination_pass(&f, "B")];
    }
    let recovered = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered.stats.downloaded, 1,
        "quiet provider must retry owned B after config drift: {recovered:?}"
    );
    assert_eq!(gets.load(Ordering::SeqCst), before_gets + 1);
    assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), before_ranks);
    assert_eq!(std::fs::read(&b).unwrap(), bytes);
    assert_eq!(std::fs::read(&a).unwrap(), bytes);
    assert_eq!(std::fs::metadata(&a).unwrap().modified().unwrap(), a_mtime);
    assert!(!f.config.directory.join("changed-template").exists());
    let verified: bool = f.db.acquire_lock("original B owner completed").unwrap()
        .query_row("SELECT verified_media FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child='asset-m' AND path=?3",
            rusqlite::params![original_b.0,original_b.1,original_b.2],|r|r.get(0)).unwrap();
    assert!(verified);
    let quiet = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(quiet.stats.downloaded, 0, "{quiet:?}");
    assert_eq!(gets.load(Ordering::SeqCst), before_gets + 1);
    assert_eq!(f.session.rank_calls.load(Ordering::SeqCst), before_ranks);
    assert_eq!(std::fs::metadata(&a).unwrap().modified().unwrap(), a_mtime);
    f.preserved().await;
}
