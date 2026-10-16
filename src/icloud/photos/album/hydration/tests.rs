use crate::icloud::photos::album::test_support::{
    canned_changes_page, changes_asset, changes_master, make_album_with_session, test_records,
};
use crate::icloud::photos::album::{PhotoAlbum, PhotoAlbumConfig};
use crate::icloud::photos::asset::PhotoAsset;
use crate::icloud::photos::session::PhotosSession;
use crate::retry::RetryConfig;
use crate::test_helpers::{MockPhotosSession, mock_photo_query_page};
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn container_relation_record(container_id: &str, item_id: &str) -> Value {
    json!({
        "recordName": format!("relation-{item_id}"),
        "recordType": "CPLContainerRelation",
        "fields": {
            "containerId": {"value": container_id, "type": "STRING"},
            "itemId": {"value": item_id, "type": "STRING"}
        },
        "recordChangeTag": "ct-relation"
    })
}

fn changes_page_for_zone(records: Vec<Value>, zone_name: &str, sync_token: &str) -> Value {
    json!({
        "zones": [{
            "zoneID": {"zoneName": zone_name, "ownerRecordName": "_defaultOwner"},
            "syncToken": sync_token,
            "moreComing": false,
            "records": records
        }]
    })
}

#[derive(Clone, Copy, Debug)]
enum CrossZoneSessionKind {
    Owner,
    Source,
    EmptySource,
}

#[derive(Clone, Debug)]
struct CrossZoneSession {
    kind: CrossZoneSessionKind,
    owner_query_calls: Arc<std::sync::atomic::AtomicUsize>,
    owner_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
    source_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl CrossZoneSession {
    fn new(
        kind: CrossZoneSessionKind,
        owner_query_calls: Arc<std::sync::atomic::AtomicUsize>,
        owner_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
        source_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            kind,
            owner_query_calls,
            owner_changes_calls,
            source_changes_calls,
        }
    }
}

#[async_trait::async_trait]
impl PhotosSession for CrossZoneSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/query") {
            assert!(
                matches!(self.kind, CrossZoneSessionKind::Owner),
                "only the owner album should use records/query"
            );
            let call = self
                .owner_query_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                return Ok(mock_photo_query_page("master-owner", Some("owner-token")));
            }
            return Ok(json!({"records": [], "syncToken": "owner-token"}));
        }

        if url.contains("/changes/zone") {
            return match self.kind {
                CrossZoneSessionKind::Owner => {
                    self.owner_changes_calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(changes_page_for_zone(
                        vec![
                            container_relation_record("album-container", "asset-master-owner"),
                            container_relation_record("album-container", "asset-master-shared"),
                        ],
                        "PrimarySync",
                        "owner-changes-token",
                    ))
                }
                CrossZoneSessionKind::Source => {
                    self.source_changes_calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(changes_page_for_zone(
                        test_records("master-shared"),
                        "SharedSync-abc",
                        "source-changes-token",
                    ))
                }
                CrossZoneSessionKind::EmptySource => {
                    self.source_changes_calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(changes_page_for_zone(
                        Vec::new(),
                        "SharedSync-abc",
                        "source-changes-token",
                    ))
                }
            };
        }

        anyhow::bail!("unexpected URL: {url}")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn make_cross_zone_album(
    zone_name: &str,
    session: impl PhotosSession + 'static,
    container_id: Option<Arc<str>>,
    cross_zone_sources: Vec<PhotoAlbum>,
) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": zone_name})),
            retry_config: RetryConfig::default(),
            container_id,
            cross_zone_sources,
        },
        Box::new(session),
    )
}

type CrossZoneTestSetup = (
    PhotoAlbum,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicUsize>,
);

fn make_cross_zone_owner_album() -> CrossZoneTestSetup {
    make_cross_zone_owner_album_with_source_kind(CrossZoneSessionKind::Source)
}

fn make_cross_zone_owner_album_with_source_kind(
    source_kind: CrossZoneSessionKind,
) -> CrossZoneTestSetup {
    let owner_query_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let owner_changes_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source_changes_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let owner_session = CrossZoneSession::new(
        CrossZoneSessionKind::Owner,
        Arc::clone(&owner_query_calls),
        Arc::clone(&owner_changes_calls),
        Arc::clone(&source_changes_calls),
    );
    let source_session = CrossZoneSession::new(
        source_kind,
        Arc::clone(&owner_query_calls),
        Arc::clone(&owner_changes_calls),
        Arc::clone(&source_changes_calls),
    );
    let source = make_cross_zone_album("SharedSync-abc", source_session, None, Vec::new());
    let owner = make_cross_zone_album(
        "PrimarySync",
        owner_session,
        Some(Arc::from("album-container")),
        vec![source],
    );
    (
        owner,
        owner_query_calls,
        owner_changes_calls,
        source_changes_calls,
    )
}

#[tokio::test]
async fn photo_stream_hydrates_named_album_members_from_other_zones() {
    use tokio_stream::StreamExt;

    let (owner, owner_query_calls, owner_changes_calls, source_changes_calls) =
        make_cross_zone_owner_album();

    let (stream, token_rx) = owner.photo_stream_with_token(None, Some(2), 1);
    tokio::pin!(stream);

    let mut assets = Vec::new();
    while let Some(result) = stream.next().await {
        assets.push(result.expect("photo asset should be Ok"));
    }

    assert_eq!(assets.len(), 2);
    assert!(assets.iter().any(|asset| asset.id() == "master-owner"
        && asset.asset_record_name() == "asset-master-owner"
        && asset.source_zone().is_none()));
    assert!(assets.iter().any(|asset| asset.id() == "master-shared"
        && asset.asset_record_name() == "asset-master-shared"
        && asset.source_zone() == Some("SharedSync-abc")));
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("owner-token"),
        "fully resolved cross-zone hydration can keep the owner-zone token"
    );
    assert_eq!(
        owner_query_calls.load(std::sync::atomic::Ordering::SeqCst),
        6,
        "base enumeration should stop after the owner page plus finite empty-tail proof"
    );
    assert_eq!(
        owner_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "owner zone relation scan should run only when base enumeration is short"
    );
    assert_eq!(
        source_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "source zone scan should be bounded to missing relation members"
    );
}

#[tracing_test::traced_test]
#[tokio::test]
async fn photo_stream_warns_but_continues_for_unresolved_relation_members() {
    use tokio_stream::StreamExt;

    let (owner, _owner_query_calls, owner_changes_calls, source_changes_calls) =
        make_cross_zone_owner_album_with_source_kind(CrossZoneSessionKind::EmptySource);

    let (stream, token_rx) = owner.photo_stream_with_token(None, Some(2), 1);
    tokio::pin!(stream);

    let mut assets = Vec::new();
    while let Some(result) = stream.next().await {
        assets.push(result.expect("unresolved relation records should warn, not error"));
    }

    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].id(), "master-owner");
    assert_eq!(
        token_rx.await.expect("sync token sender"),
        None,
        "unresolved relation records suppress owner-zone token advancement"
    );
    assert_eq!(
        owner_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "owner relation scan should still run"
    );
    assert_eq!(
        source_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "source zone scan should try to resolve relation members"
    );
    assert!(logs_contain("unresolved=1"));
    assert!(logs_contain("Album has unresolved relation records"));
}

#[tokio::test]
async fn photo_stream_recent_limit_does_not_cross_zone_over_hydrate() {
    use tokio_stream::StreamExt;

    let (owner, owner_query_calls, owner_changes_calls, source_changes_calls) =
        make_cross_zone_owner_album();

    let (stream, token_rx) = owner.photo_stream_with_token(Some(1), Some(2), 1);
    tokio::pin!(stream);

    let mut assets = Vec::new();
    while let Some(result) = stream.next().await {
        assets.push(result.expect("photo asset should be Ok"));
    }

    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].id(), "master-owner");
    assert_eq!(
        token_rx.await.expect("sync token sender"),
        None,
        "recent-limited streams stop before the full owner-zone checkpoint"
    );
    assert_eq!(
        owner_query_calls.load(std::sync::atomic::Ordering::SeqCst),
        6,
        "recent limit should stop after the requested item and finite owner-zone probe"
    );
    assert_eq!(
        owner_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "recent-limited streams should not widen into full relation scans"
    );
    assert_eq!(
        source_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "recent-limited streams should not scan source zones"
    );
}

#[tokio::test]
async fn hydrate_matching_assets_from_changes_stops_after_all_targets_match() {
    let records = vec![
        changes_master("master-1"),
        changes_asset("asset-1", "master-1"),
    ];
    let mock = MockPhotosSession::new()
        .ok(json!({"records":[]}))
        .ok(canned_changes_page(&records, "token-page1", true));
    let album = make_album_with_session(100, Box::new(mock));
    let mut missing = FxHashSet::default();
    missing.insert("asset-1".to_string());

    let matched = album
        .hydrate_matching_assets_from_changes(&mut missing)
        .await
        .expect("matching hydrate should stop without fetching the next page");

    assert!(missing.is_empty());
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].asset_record_name(), "asset-1");
}

#[tokio::test]
async fn hydrate_matching_master_assets_collects_every_live_sibling() {
    let records = vec![
        changes_master("master-target"),
        changes_asset("asset-target-a", "master-target"),
        changes_asset("asset-target-b", "master-target"),
        changes_master("master-other"),
        changes_asset("asset-other", "master-other"),
    ];
    let mock = MockPhotosSession::new().ok(canned_changes_page(&records, "token-final", false));
    let album = make_album_with_session(100, Box::new(mock));
    let masters = FxHashSet::from_iter(["master-target".to_string()]);

    let matched = album
        .hydrate_matching_master_assets_from_changes(&masters, &CancellationToken::new())
        .await
        .expect("master hydration");

    assert_eq!(matched.len(), 2);
    assert!(matched.iter().all(|asset| asset.id() == "master-target"));
    assert!(
        matched
            .iter()
            .all(|asset| asset.source_zone() == Some("PrimarySync"))
    );
    let asset_record_names: FxHashSet<&str> =
        matched.iter().map(PhotoAsset::asset_record_name).collect();
    assert_eq!(
        asset_record_names,
        FxHashSet::from_iter(["asset-target-a", "asset-target-b"])
    );
}

#[tokio::test]
async fn hydrate_matching_master_assets_retains_hidden_siblings_but_not_deleted_children() {
    let mut hidden = changes_asset("asset-hidden", "master-target");
    hidden["fields"]["isHidden"] = json!({"value": 1, "type": "INT64"});
    let mut soft_deleted = hidden.clone();
    soft_deleted["recordName"] = json!("asset-soft-deleted");
    soft_deleted["fields"]["isDeleted"] = json!({"value": 1, "type": "INT64"});
    let mut hard_deleted = hidden.clone();
    hard_deleted["recordName"] = json!("asset-hard-deleted");
    hard_deleted["deleted"] = json!(true);
    let page1 = vec![
        changes_master("master-target"),
        changes_asset("asset-visible", "master-target"),
        soft_deleted,
        hard_deleted,
    ];
    let mock = MockPhotosSession::new()
        .ok(canned_changes_page(&page1, "token-page1", true))
        .ok(canned_changes_page(&[hidden], "token-final", false));
    let album = make_album_with_session(100, Box::new(mock));
    let masters = FxHashSet::from_iter(["master-target".to_string()]);

    let matched = album
        .hydrate_matching_master_assets_from_changes(&masters, &CancellationToken::new())
        .await
        .expect("master hydration must include the later hidden sibling");

    assert_eq!(matched.len(), 2);
    let names: FxHashSet<_> = matched.iter().map(PhotoAsset::asset_record_name).collect();
    assert_eq!(
        names,
        FxHashSet::from_iter(["asset-visible", "asset-hidden"])
    );
    assert!(matched.iter().any(|asset| asset.metadata().is_hidden));
    assert!(
        matched
            .iter()
            .all(|asset| asset.source_zone() == Some("PrimarySync"))
    );
}

#[tokio::test]
async fn hydrate_matching_master_assets_scans_later_sibling_pages() {
    let page1 = vec![
        changes_master("master-target"),
        changes_asset("asset-target-a", "master-target"),
    ];
    let page2 = vec![changes_asset("asset-target-b", "master-target")];
    let mock = MockPhotosSession::new()
        .ok(canned_changes_page(&page1, "token-page1", true))
        .ok(canned_changes_page(&page2, "token-final", false));
    let album = make_album_with_session(100, Box::new(mock));
    let masters = FxHashSet::from_iter(["master-target".to_string()]);

    let matched = album
        .hydrate_matching_master_assets_from_changes(&masters, &CancellationToken::new())
        .await
        .expect("master hydration should scan every page");

    let asset_record_names: FxHashSet<&str> =
        matched.iter().map(PhotoAsset::asset_record_name).collect();
    assert_eq!(
        asset_record_names,
        FxHashSet::from_iter(["asset-target-a", "asset-target-b"])
    );
}

#[derive(Clone, Copy)]
enum HydrationLookupReply {
    Complete,
    MissingMaster,
    ForeignOwner,
    ChangedPair,
    Deleted,
    Omitted,
    Authentication,
}

#[derive(Clone)]
struct HydrationLookupSession {
    catalog: Arc<HashMap<String, Value>>,
    reply: HydrationLookupReply,
    requests: Arc<std::sync::Mutex<Vec<Value>>>,
    scans: Arc<std::sync::atomic::AtomicUsize>,
    scan_catalog: bool,
}
impl HydrationLookupSession {
    fn new(reply: HydrationLookupReply, masters: usize) -> Self {
        let mut catalog = HashMap::new();
        for i in 0..masters {
            for record in test_records(&format!("master-{i}")) {
                catalog.insert(record["recordName"].as_str().unwrap().to_owned(), record);
            }
        }
        Self {
            catalog: Arc::new(catalog),
            reply,
            requests: Arc::default(),
            scans: Arc::default(),
            scan_catalog: false,
        }
    }
}
#[async_trait::async_trait]
impl PhotosSession for HydrationLookupSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let body: Value = serde_json::from_str(&body)?;
        if url.contains("/records/lookup?") {
            self.requests.lock().unwrap().push(body.clone());
            if matches!(self.reply, HydrationLookupReply::Authentication) {
                return Err(crate::icloud::photos::session::HttpStatusError {
                    status: 401,
                    url: "https://example.invalid".to_owned(),
                    retry_after: None,
                    body: None,
                }
                .into());
            }
            let names = body["records"].as_array().unwrap();
            assert!(names.len() <= 200);
            let mut records = Vec::new();
            for name in names {
                let id = name["recordName"].as_str().unwrap();
                if matches!(self.reply, HydrationLookupReply::Omitted)
                    || (matches!(self.reply, HydrationLookupReply::MissingMaster)
                        && !id.starts_with("asset-"))
                {
                    continue;
                }
                if let Some(record) = self.catalog.get(id) {
                    let mut record = record.clone();
                    if matches!(self.reply, HydrationLookupReply::ForeignOwner) {
                        record["zoneID"] =
                            json!({"zoneName":"PrimarySync","ownerRecordName":"foreign-owner"});
                    }
                    if matches!(self.reply, HydrationLookupReply::ChangedPair)
                        && names.len() > 1
                        && id.starts_with("asset-")
                    {
                        record["fields"]["masterRef"]["value"]["recordName"] =
                            json!("changed-master");
                    }
                    if matches!(self.reply, HydrationLookupReply::Deleted) {
                        record = json!({"recordName":id,"serverErrorCode":"UNKNOWN_ITEM"});
                    }
                    records.push(record);
                }
            }
            return Ok(json!({"records":records}));
        }
        assert!(url.contains("/changes/zone?"));
        self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(
            json!({"zones":[{"zoneID":body["zones"][0]["zoneID"],"syncToken":"scan-eof","moreComing":false,"records":if self.scan_catalog {self.catalog.values().cloned().collect::<Vec<_>>()} else {Vec::new()}}]}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn issue958_direct_relation_hydration_requires_complete_scoped_pairs() {
    use std::sync::atomic::Ordering;
    for reply in [
        HydrationLookupReply::Complete,
        HydrationLookupReply::MissingMaster,
        HydrationLookupReply::ForeignOwner,
        HydrationLookupReply::ChangedPair,
        HydrationLookupReply::Deleted,
        HydrationLookupReply::Omitted,
    ] {
        let session = HydrationLookupSession::new(reply, 1);
        let album = make_album_with_session(100, Box::new(session.clone()));
        let mut missing = FxHashSet::from_iter(["asset-master-0".to_owned()]);
        let assets = album
            .hydrate_matching_assets_from_changes(&mut missing)
            .await
            .unwrap();
        let complete = matches!(reply, HydrationLookupReply::Complete);
        assert_eq!(assets.len(), usize::from(complete));
        assert_eq!(missing.is_empty(), complete);
        assert_eq!(session.scans.load(Ordering::SeqCst), usize::from(!complete));
        let requests = session.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            if matches!(
                reply,
                HydrationLookupReply::Complete
                    | HydrationLookupReply::MissingMaster
                    | HydrationLookupReply::ChangedPair
            ) {
                2
            } else {
                1
            }
        );
        if complete {
            assert_eq!(assets[0].asset_record_name(), "asset-master-0");
            assert_eq!(assets[0].source_zone(), Some("PrimarySync"));
        }
    }
}

#[tokio::test]
async fn issue958_direct_relation_hydration_batches_and_keeps_missing_targets() {
    use std::sync::atomic::Ordering;
    let session = HydrationLookupSession::new(HydrationLookupReply::Complete, 201);
    let album = make_album_with_session(100, Box::new(session.clone()));
    let mut missing: FxHashSet<_> = (0..201).map(|i| format!("asset-master-{i}")).collect();
    missing.insert("asset-omitted".to_owned());
    let assets = album
        .hydrate_matching_assets_from_changes(&mut missing)
        .await
        .unwrap();
    assert_eq!(assets.len(), 201);
    assert_eq!(missing, FxHashSet::from_iter(["asset-omitted".to_owned()]));
    assert_eq!(session.requests.lock().unwrap().len(), 6);
    assert_eq!(session.scans.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn issue958_direct_cross_zone_lookup_preserves_source_priority() {
    use std::sync::atomic::Ordering;
    let owner = HydrationLookupSession::new(HydrationLookupReply::Omitted, 0);
    let mut source = HydrationLookupSession::new(HydrationLookupReply::Complete, 1);
    let mut catalog = (*source.catalog).clone();
    catalog.get_mut("asset-master-0").unwrap()["fields"]["masterRef"]["value"]["zoneID"]["zoneName"] =
        json!("SharedSync-synthetic");
    source.catalog = Arc::new(catalog);
    let source_album =
        make_cross_zone_album("SharedSync-synthetic", source.clone(), None, Vec::new());
    let album = make_album_with_session(100, Box::new(owner.clone()))
        .with_cross_zone_sources(vec![source_album]);
    let mut missing = FxHashSet::from_iter(["asset-master-0".to_owned()]);
    let assets = album
        .hydrate_matching_assets_from_changes(&mut missing)
        .await
        .unwrap();
    assert!(missing.is_empty());
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].source_zone(), Some("SharedSync-synthetic"));
    assert_eq!(owner.requests.lock().unwrap().len(), 1);
    assert_eq!(source.requests.lock().unwrap().len(), 2);
    assert_eq!(
        owner.scans.load(Ordering::SeqCst) + source.scans.load(Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn issue958_direct_relation_auth_failure_stops_without_scanning() {
    use std::sync::atomic::Ordering;
    let session = HydrationLookupSession::new(HydrationLookupReply::Authentication, 1);
    let album = make_album_with_session(100, Box::new(session.clone()));
    let mut missing = FxHashSet::from_iter(["asset-master-0".to_owned()]);
    let error = album
        .hydrate_matching_assets_from_changes(&mut missing)
        .await
        .unwrap_err();
    assert!(crate::icloud::photos::session::is_session_error(&error));
    assert_eq!(missing.len(), 1);
    assert_eq!(session.requests.lock().unwrap().len(), 1);
    assert_eq!(session.scans.load(Ordering::SeqCst), 0);
}

#[test]
fn issue958_hydration_sharing_requires_request_context_and_complete_scope() {
    let album = make_album_with_session(
        100,
        Box::new(HydrationLookupSession::new(
            HydrationLookupReply::Omitted,
            0,
        )),
    );
    let mut other = album.clone_for_task_without_sources();
    assert!(album.shares_hydration_sources_with(&other));
    other.params = Arc::new((*album.params).clone());
    assert!(!album.shares_hydration_sources_with(&other));
    other = album.clone_for_task_without_sources();
    other.zone_id = Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"foreign-owner"}));
    assert!(!album.shares_hydration_sources_with(&other));
    other = album.clone_for_task_without_sources();
    other.service_endpoint = Arc::from("https://example.invalid/shared");
    assert!(!album.shares_hydration_sources_with(&other));
    other = album.clone_for_task_without_sources();
    other
        .cross_zone_sources
        .push(album.clone_for_task_without_sources());
    assert!(!album.shares_hydration_sources_with(&other));
    let mut first = album.clone_for_task_without_sources();
    let mut source_a = album.clone_for_task_without_sources();
    source_a.zone_id = Arc::new(json!({"zoneName":"SharedSync-a"}));
    let mut source_b = album.clone_for_task_without_sources();
    source_b.zone_id = Arc::new(json!({"zoneName":"SharedSync-b"}));
    let mut reversed = album.clone_for_task_without_sources();
    reversed.cross_zone_sources = vec![
        source_b.clone_for_task_without_sources(),
        source_a.clone_for_task_without_sources(),
    ];
    first.cross_zone_sources = vec![source_a, source_b];
    assert!(!first.shares_hydration_sources_with(&reversed));
}

#[tokio::test]
async fn issue958_inconclusive_owner_lookup_cannot_prefer_cross_zone_namesake() {
    let mut owner = HydrationLookupSession::new(HydrationLookupReply::Omitted, 1);
    owner.scan_catalog = true;
    let source = HydrationLookupSession::new(HydrationLookupReply::Complete, 1);
    let album =
        make_album_with_session(100, Box::new(owner.clone())).with_cross_zone_sources(vec![
            make_cross_zone_album("SharedSync-synthetic", source.clone(), None, Vec::new()),
        ]);
    let mut missing = FxHashSet::from_iter(["asset-master-0".to_owned()]);
    let assets = album
        .hydrate_matching_assets_from_changes(&mut missing)
        .await
        .unwrap();
    assert!(missing.is_empty());
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].source_zone(), Some("PrimarySync"));
    assert_eq!(owner.scans.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(source.requests.lock().unwrap().is_empty());
    assert_eq!(source.scans.load(std::sync::atomic::Ordering::SeqCst), 0);
}
