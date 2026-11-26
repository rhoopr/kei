use super::*;
use crate::icloud::photos::album::test_support::{
    default_zone, make_album_with_session, test_asset_record_for, test_master_record,
};
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::session::PhotosSession;
use crate::state::SqliteStateDb;
use crate::state::db::account::AccountOwner;
use anyhow::Result;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[test]
fn retained_album_proof_keeps_single_link_encoding_and_requires_the_complete_set() {
    let zone = default_zone();
    let pair_member = RawValue::from_string("{}".into()).unwrap();
    let links: Vec<_> = ["album-a", "album-b"]
        .into_iter()
        .map(|container| ReplayAlbumLinkLookup {
            record_name: format!("photo-IN-{container}"),
            container: container.to_owned(),
        })
        .collect();
    let tombstones: Vec<_> = links
        .iter()
        .map(|link| {
            RawValue::from_string(json!({"recordName":link.record_name,"deleted":true}).to_string())
                .unwrap()
        })
        .collect();
    let matches = |body: &[u8], scope: &[ReplayAlbumLinkLookup]| {
        replay_album_confirmation_matches(body, scope, "photo", &zone, ReplayAlbumLink::Removed)
            .unwrap()
    };
    let single =
        pair_confirmation_body(&pair_member, &pair_member, &[&tombstones[0]], None).unwrap();
    let old_shape = format!(
        r#"{{"records":[{{}},{{}}],"albumMembership":{}}}"#,
        tombstones[0]
    );
    assert_eq!(single, old_shape.as_bytes());
    assert!(matches(&single, &links[..1]));
    assert!(!matches(&single, &links));
    let both = pair_confirmation_body(
        &pair_member,
        &pair_member,
        &[&tombstones[0], &tombstones[1]],
        None,
    )
    .unwrap();
    assert!(matches(&both, &links));
    assert!(!matches(&both, &links[..1]));
}

#[derive(Clone, Default)]
struct LookupSession {
    response: Arc<Mutex<Vec<u8>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl LookupSession {
    fn returning(response: String) -> Self {
        Self {
            response: Arc::new(Mutex::new(response.into_bytes())),
            requests: Arc::default(),
        }
    }

    fn recorded_names(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one bounded lookup request");
        requests[0]["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| record["recordName"].as_str().unwrap().to_owned())
            .collect()
    }
}

#[async_trait::async_trait]
impl PhotosSession for LookupSession {
    async fn post(&self, url: &str, body: String, headers: &[(&str, &str)]) -> Result<Value> {
        Ok(serde_json::from_slice(
            &self.post_changes_body(url, body, headers).await?,
        )?)
    }

    async fn post_changes_body(
        &self,
        _url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_str(&body)?);
        Ok(self.response.lock().unwrap().clone())
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn album(session: &LookupSession) -> PhotoAlbum {
    make_album_with_session(100, Box::new(session.clone()))
}

fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-provider"}}))
            .unwrap(),
    )
    .unwrap()
}

async fn bound_album(session: &LookupSession) -> (PhotoAlbum, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&directory.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let capture = ShadowCapture::new(db, owner(), "com");
    let mut album = album(session);
    album.set_shadow_capture(capture, Arc::from("private"));
    (album, directory)
}

fn observed(child: &str, master: &str) -> PhotoAsset {
    PhotoAsset::new(
        test_master_record(master),
        test_asset_record_for(child, master),
    )
}

fn master_record(name: &str, url: &str) -> String {
    format!(
        r#"{{"recordName":"{name}","recordType":"CPLMaster","recordChangeTag":"tag-{name}","fields":{{"filenameEnc":{{"value":"photo.jpg","type":"STRING"}},"resOriginalRes":{{"type":"ASSETID","value":{{"downloadURL":"{url}","size":1024,"fileChecksum":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=","referenceChecksum":"reference","wrappingKey":"wrapping"}}}},"resOriginalFileType":{{"value":"public.jpeg"}},"itemType":{{"value":"public.jpeg"}}}},"unknownRecordField":{{"value":1.2300e+30}}}}"#
    )
}

fn child_record(name: &str, master: &str) -> String {
    format!(
        r#"{{"recordName":"{name}","recordType":"CPLAsset","recordChangeTag":"tag-{name}","fields":{{"masterRef":{{"value":{{"recordName":"{master}","zoneID":{{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}}},"type":"REFERENCE"}},"assetDate":{{"value":1700000000000,"type":"TIMESTAMP"}},"addedDate":{{"value":1700000000000,"type":"TIMESTAMP"}}}}}}"#
    )
}

fn response(records: &[String]) -> String {
    format!("{{\"records\":[{}]}}", records.join(","))
}

async fn rejected(records: Vec<String>, observed: &[PhotoAsset]) -> anyhow::Error {
    let session = LookupSession::returning(response(&records));
    let (album, _directory) = bound_album(&session).await;
    let observed: Vec<&PhotoAsset> = observed.iter().collect();
    match album.confirm_unfiled_batch(&observed).await {
        Ok(_) => panic!("the batch must refuse rather than return partial evidence"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn confirm_unfiled_batch_orders_pairs_and_dedupes_shared_masters() {
    let shared_master = master_record("master-shared", "https://p01.icloud-content.com/shared");
    let session = LookupSession::returning(response(&[
        child_record("asset-b", "master-b"),
        master_record("master-b", "https://p01.icloud-content.com/b"),
        shared_master.clone(),
        child_record("asset-a2", "master-shared"),
        child_record("asset-a1", "master-shared"),
    ]));
    let (album, _directory) = bound_album(&session).await;
    let a1 = observed("asset-a1", "master-shared");
    let a2 = observed("asset-a2", "master-shared");
    let b = observed("asset-b", "master-b");

    let current = album.confirm_unfiled_batch(&[&a1, &a2, &b]).await.unwrap();

    assert_eq!(current.len(), 3, "results follow observation order");
    assert_eq!(
        session.recorded_names(),
        [
            "asset-a1",
            "master-shared",
            "asset-a2",
            "asset-b",
            "master-b"
        ]
    );
    for (current, child, master) in current
        .iter()
        .zip(["asset-a1", "asset-a2", "asset-b"])
        .zip(["master-shared", "master-shared", "master-b"])
        .map(|((current, child), master)| (current, child, master))
    {
        let value = changes_json::parse(&current.body).unwrap();
        let records = value["records"].as_array().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["recordName"].as_str(), Some(child));
        assert_eq!(records[1]["recordName"].as_str(), Some(master));
        assert_eq!(current.asset.asset_record_name(), child);
        assert_eq!(current.asset.id(), master);
        assert_eq!(
            current_asset(&current.body, child, master, &default_zone())
                .unwrap()
                .id(),
            master
        );
        assert!(
            crate::icloud::photos::transport_evidence::classify_lookup_confirmation(
                &current.body,
                child,
                master,
            )
            .unwrap()
            .is_some(),
            "retained evidence must classify the two-record confirmation"
        );
        assert!(
            String::from_utf8_lossy(&current.body)
                .contains(r#""unknownRecordField":{"value":1.2300e+30}"#),
            "provider record bytes stay verbatim"
        );
    }
    assert!(
        String::from_utf8_lossy(&current[0].body).contains(&shared_master),
        "the shared master slice is copied exactly"
    );
}

#[tokio::test]
async fn confirm_unfiled_batch_rejects_incomplete_extra_and_duplicate_members() {
    let a1 = observed("asset-a1", "master-shared");
    let a2 = observed("asset-a2", "master-shared");
    let observed = [a1, a2];

    let missing = vec![
        child_record("asset-a1", "master-shared"),
        master_record("master-shared", "https://p01.icloud-content.com/shared"),
    ];
    let extra = vec![
        child_record("asset-a1", "master-shared"),
        master_record("master-shared", "https://p01.icloud-content.com/shared"),
        child_record("asset-a2", "master-shared"),
        master_record("extra", "https://p01.icloud-content.com/extra"),
    ];
    let duplicate = vec![
        child_record("asset-a1", "master-shared"),
        master_record("master-shared", "https://p01.icloud-content.com/shared"),
        child_record("asset-a1", "master-shared"),
    ];
    let wrong_type = vec![
        child_record("asset-a1", "master-shared"),
        child_record("asset-a2", "master-shared"),
        master_record("master-shared", "https://p01.icloud-content.com/shared")
            .replace(r#""recordType":"CPLMaster""#, r#""recordType":"CPLAsset""#),
    ];

    for (case, records) in [
        ("missing", missing),
        ("extra", extra),
        ("duplicate", duplicate),
        ("wrong type", wrong_type),
    ] {
        let error = rejected(records, &observed).await;
        assert!(
            !error.to_string().is_empty(),
            "case {case} must produce a safe refusal"
        );
    }
}

#[tokio::test]
async fn confirm_unfiled_batch_rejects_records_outside_the_bound_zone() {
    let a1 = observed("asset-a1", "master-shared");
    let a2 = observed("asset-a2", "master-shared");
    let observed = [a1, a2];
    let records = vec![
        child_record("asset-a1", "master-shared").replace("PrimarySync", "OtherSync"),
        master_record("master-shared", "https://p01.icloud-content.com/shared"),
        child_record("asset-a2", "master-shared"),
    ];
    rejected(records, &observed).await;
}

#[tokio::test]
async fn confirm_unfiled_batch_rejects_observations_bound_to_another_source() {
    let session = LookupSession::returning(response(&[]));
    let (album, _directory) = bound_album(&session).await;
    let a1 = observed("asset-a1", "master-shared").with_source_zone(Arc::from("SharedSync-other"));
    let a2 = observed("asset-a2", "master-shared").with_source_zone(Arc::from("SharedSync-other"));
    let error = match album.confirm_unfiled_batch(&[&a1, &a2]).await {
        Ok(_) => panic!("another source must not be confirmed"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "Current confirmation observation is bound to another source"
    );
    assert!(session.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn confirm_unfiled_batch_rejects_an_unbound_album_before_lookup() {
    let session = LookupSession::returning(response(&[]));
    let album = album(&session);
    let a1 = observed("asset-a1", "master-shared");
    let a2 = observed("asset-a2", "master-shared");
    let error = match album.confirm_unfiled_batch(&[&a1, &a2]).await {
        Ok(_) => panic!("an unbound album must not send a batch"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "Selection owner is not authenticated");
    assert!(session.requests.lock().unwrap().is_empty());
}

#[test]
fn derived_pair_proofs_share_one_page_budget_across_the_batch() {
    let limit = crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES;
    let mut total = 0usize;
    add_pair_proof_bytes(&mut total, limit / 2).unwrap();
    add_pair_proof_bytes(&mut total, limit / 2).unwrap();
    assert!(add_pair_proof_bytes(&mut total, 1).is_err());
}

#[tokio::test]
async fn confirm_unfiled_batch_refuses_shared_master_expansion_over_the_page_budget() {
    let padding = "x".repeat(crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES / 3 + 1024);
    let shared = master_record("master-shared", "https://p01.icloud-content.com/shared").replace(
        r#""unknownRecordField":{"value":1.2300e+30}"#,
        &format!(r#""padding":"{padding}""#),
    );
    let session = LookupSession::returning(response(&[
        shared,
        child_record("asset-a", "master-shared"),
        child_record("asset-b", "master-shared"),
        child_record("asset-c", "master-shared"),
    ]));
    let (album, _directory) = bound_album(&session).await;
    let a = observed("asset-a", "master-shared");
    let b = observed("asset-b", "master-shared");
    let c = observed("asset-c", "master-shared");
    let error = match album.confirm_unfiled_batch(&[&a, &b, &c]).await {
        Ok(_) => panic!("derived proof expansion must stay within the page budget"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("provider evidence byte limit"),
        "{error}"
    );
}

#[tokio::test]
async fn confirm_unfiled_batch_is_bounded_by_the_exposed_limit() {
    assert_eq!(PhotoAlbum::CONFIRMATION_BATCH_SIZE, 50);
    let session = LookupSession::returning(response(&[]));
    let (album, _directory) = bound_album(&session).await;
    let assets: Vec<PhotoAsset> = (0..=PhotoAlbum::CONFIRMATION_BATCH_SIZE)
        .map(|index| observed(&format!("asset-{index}"), &format!("master-{index}")))
        .collect();
    let observed: Vec<&PhotoAsset> = assets.iter().collect();
    let error = match album.confirm_unfiled_batch(&observed).await {
        Ok(_) => panic!("the exposed batch limit must refuse overflow"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "Current confirmation batch exceeds the provider lookup bound"
    );
    assert!(session.requests.lock().unwrap().is_empty());
}
