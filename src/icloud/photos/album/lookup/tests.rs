use crate::icloud::photos::album::test_support::{
    default_zone, make_album_with_session, test_asset_record_for, test_master_record,
};
use crate::icloud::photos::session::PhotosSession;
use crate::retry::RetryConfig;
use crate::test_helpers::MockPhotosSession;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;

use super::{
    ProviderLookupError, ProviderRecordId, RecordLookupRequest, RecordResolution,
    asset_identity_diagnostic, classify_provider_lookup_error,
};

fn lookup_request(state_id: &str, master: &str, asset: &str) -> RecordLookupRequest {
    RecordLookupRequest::paired(
        ProviderRecordId::new(state_id),
        ProviderRecordId::new(master),
        ProviderRecordId::new(asset),
    )
}

/// Live Phase 0 capability probe for the private CloudKit database.
///
/// Run single-threaded with the maintainer test account:
/// `cargo test live_targeted_record_lookup_distinguishes_present_and_missing -- --ignored --test-threads=1`
#[tokio::test]
#[ignore = "requires live iCloud credentials and a trusted session"]
async fn live_targeted_record_lookup_distinguishes_present_and_missing() {
    let _ = dotenvy::from_filename(".env");
    let username = std::env::var("ICLOUD_USERNAME").expect("ICLOUD_USERNAME must be set");
    let password = std::env::var("ICLOUD_PASSWORD").expect("ICLOUD_PASSWORD must be set");
    let cookie_dir = std::env::var_os("ICLOUD_TEST_COOKIE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(".test-cookies"));
    let password_provider: crate::password::PasswordProvider =
        Arc::new(move || Some(crate::password::SecretString::from(password.clone())));
    let auth = crate::auth::authenticate(
        &cookie_dir,
        &username,
        &password_provider,
        "com",
        None,
        None,
        None,
    )
    .await
    .expect("authenticate live lookup probe");
    let (_session, mut service) = crate::commands::init_photos_service(
        auth,
        RetryConfig::default(),
        crate::personality::Mode::Off,
    )
    .await
    .expect("initialize live Photos service");
    let album = service
        .get_library(crate::icloud::photos::PRIMARY_ZONE_NAME)
        .await
        .expect("resolve primary library")
        .all();
    let asset = album
        .photos(Some(1))
        .await
        .expect("enumerate one live asset")
        .into_iter()
        .next()
        .expect("live account must contain at least one photo");
    let missing_id = "kei-phase0-record-that-does-not-exist";
    let master_only_state_id = "live-master-only";
    let requests = [
        lookup_request(asset.id(), asset.id(), asset.asset_record_name()),
        RecordLookupRequest::asset_only(ProviderRecordId::new(asset.asset_record_name())),
        RecordLookupRequest::master_only(
            ProviderRecordId::new(master_only_state_id),
            ProviderRecordId::new(asset.id()),
        ),
        RecordLookupRequest::master_only(
            ProviderRecordId::new(missing_id),
            ProviderRecordId::new(missing_id),
        ),
    ];

    let result = album.resolve_records(&requests).await;

    assert!(
        !result.complete,
        "a live master-only lookup still needs its CPLAsset pair"
    );
    assert!(matches!(
        result
            .results
            .iter()
            .find(|(id, _)| id.as_str() == asset.id()),
        Some((_, RecordResolution::Present(_)))
    ));
    assert!(matches!(
        result.results.iter().find(|(id, _)|
            id.as_str() == asset.asset_record_name()
        ),
        Some((
            _,
            RecordResolution::AssetPresent { master_record_name }
        )) if master_record_name.as_str() == asset.id()
    ));
    assert!(matches!(
        result
            .results
            .iter()
            .find(|(id, _)| id.as_str() == master_only_state_id),
        Some((_, RecordResolution::MasterPresent))
    ));
    assert!(matches!(
        result
            .results
            .iter()
            .find(|(id, _)| id.as_str() == missing_id),
        Some((
            _,
            RecordResolution::Deleted {
                deleted_at: None,
                master_family: true,
            }
        ))
    ));
}

#[tokio::test]
async fn targeted_record_lookup_scopes_zone_at_request_level() {
    #[derive(Clone)]
    struct CaptureLookupSession {
        body: Arc<Mutex<Option<Value>>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for CaptureLookupSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            assert!(url.contains("/records/lookup?"));
            *self.body.lock().unwrap() = Some(serde_json::from_str(&body)?);
            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let captured = Arc::new(Mutex::new(None));
    let album = make_album_with_session(
        100,
        Box::new(CaptureLookupSession {
            body: Arc::clone(&captured),
        }),
    );
    album
        .resolve_records(&[lookup_request("master", "master", "asset")])
        .await;

    let body = captured.lock().unwrap().clone().expect("lookup body");
    assert_eq!(body["zoneID"], default_zone());
    let records = body["records"].as_array().expect("records array");
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record.get("zoneID").is_none()));
}

#[tokio::test]
async fn targeted_master_only_lookup_resolves_explicit_legacy_deletion() {
    let response = json!({
        "records": [{
            "recordName": "legacy-master",
            "serverErrorCode": "UNKNOWN_ITEM",
            "reason": "record not found"
        }]
    });
    let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

    let batch = album
        .resolve_records(&[RecordLookupRequest::master_only(
            ProviderRecordId::new("legacy-master"),
            ProviderRecordId::new("legacy-master"),
        )])
        .await;

    assert!(batch.complete);
    assert!(matches!(
        batch.results.as_slice(),
        [(
            _,
            RecordResolution::Deleted {
                master_family: true,
                ..
            }
        )]
    ));
}

#[tokio::test]
async fn targeted_master_only_lookup_distinguishes_live_legacy_master() {
    let response = json!({
        "records": [test_master_record("legacy-master")]
    });
    let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

    let batch = album
        .resolve_records(&[RecordLookupRequest::master_only(
            ProviderRecordId::new("legacy-state"),
            ProviderRecordId::new("legacy-master"),
        )])
        .await;

    assert!(!batch.complete);
    assert!(matches!(
        batch.results.as_slice(),
        [(_, RecordResolution::MasterPresent)]
    ));
}

#[tokio::test]
async fn asset_identity_diagnostics_use_only_bounded_redacted_labels() {
    let log_dir = tempfile::tempdir().unwrap();
    let log_path = log_dir.path().join("lookup.log");
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::WARN)
        .with_writer(std::sync::Mutex::new(
            std::fs::File::create(&log_path).unwrap(),
        ))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let zone = json!({"zoneName": "PrimarySync"});
    let mut missing = test_asset_record_for("private-child", "private-master");
    missing["fields"]
        .as_object_mut()
        .unwrap()
        .remove("masterRef");
    let mut malformed = missing.clone();
    malformed["fields"]["masterRef"] = json!({"value": {"recordName": 42}});
    let mut blank = missing.clone();
    blank["fields"]["masterRef"] = json!({"value": {"recordName": " "}});
    let mut errored = test_asset_record_for("private-child", "private-master");
    errored["serverErrorCode"] = json!("ACCESS_DENIED");
    let mut decode = missing.clone();
    decode["deleted"] = json!("private-invalid-boolean");
    let cases = [
        (None, "record_omitted"),
        (
            Some(json!({"recordName": "private-child", "recordType": "private-type"})),
            "unexpected_record_type",
        ),
        (
            Some(
                json!({"recordName": "private-child", "serverErrorCode": "private-error", "reason": "private-account https://private?token=secret"}),
            ),
            "record_provider_error",
        ),
        (Some(errored), "record_access_denied"),
        (Some(decode), "record_decode_failed"),
        (Some(missing), "master_reference_missing"),
        (Some(malformed), "master_reference_malformed"),
        (Some(blank), "master_reference_malformed"),
    ];
    for (record, expected) in cases {
        let diagnostic = asset_identity_diagnostic(record.as_ref(), &zone);
        assert_eq!(diagnostic.0, expected);
        assert_eq!(
            diagnostic.1,
            if expected == "record_access_denied" {
                "same_zone"
            } else {
                "absent"
            }
        );
        assert!(!format!("{diagnostic:?}").contains("private"));
        let album = make_album_with_session(
            100,
            Box::new(
                MockPhotosSession::new()
                    .ok(json!({"records": record.into_iter().collect::<Vec<_>>()})),
            ),
        );
        let batch = album
            .resolve_records(&[RecordLookupRequest::asset_only(ProviderRecordId::new(
                "private-child",
            ))])
            .await;
        assert!(matches!(
            batch.results.as_slice(),
            [(_, RecordResolution::Unknown)]
        ));
    }
    let logs = std::fs::read_to_string(&log_path).unwrap();
    assert!(logs.contains("record_omitted"));
    assert!(logs.contains("record_decode_failed"));
    assert!(logs.contains("count=1"));
    for secret in ["private", "https://", "secret"] {
        assert!(!logs.contains(secret), "diagnostic leaked {secret}: {logs}");
    }
    let mut asset = test_asset_record_for("private-child", "private-master");
    for (reference_zone, expected) in [
        (zone.clone(), "same_zone"),
        (
            json!({"zoneName": "SharedSync-private-owner"}),
            "different_or_partial_zone",
        ),
        (json!("private-zone"), "malformed"),
    ] {
        asset["fields"]["masterRef"]["value"]["zoneID"] = reference_zone;
        assert_eq!(
            asset_identity_diagnostic(Some(&asset), &zone),
            ("master_reference_present", expected)
        );
    }
}

#[tokio::test]
async fn asset_only_delta_targeted_lookup_recovers_master_identity() {
    let mut record = test_asset_record_for("asset-present", "master-present");
    record["serverErrorCode"] = Value::Null;
    let response = json!({"records": [record]});
    let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

    let batch = album
        .resolve_records(&[RecordLookupRequest::asset_only(ProviderRecordId::new(
            "asset-present",
        ))])
        .await;

    assert!(
        !batch.complete,
        "identity lookup still needs the master pair"
    );
    assert!(matches!(
        batch.results.as_slice(),
        [(
            _,
            RecordResolution::AssetPresent { master_record_name }
        )] if master_record_name.as_str() == "master-present"
    ));
}

#[tokio::test]
async fn targeted_record_lookup_distinguishes_present_deleted_and_omitted() {
    let response = json!({
        "records": [
            test_master_record("master-present"),
            test_asset_record_for("asset-present", "master-present"),
            test_master_record("master-deleted"),
            {
                "recordName": "asset-deleted",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            }
        ]
    });
    let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));
    let batch = album
        .resolve_records(&[
            lookup_request("master-present", "master-present", "asset-present"),
            lookup_request("master-deleted", "master-deleted", "asset-deleted"),
            lookup_request("master-unknown", "master-unknown", "asset-unknown"),
        ])
        .await;

    assert!(!batch.complete, "an omitted batch member is inconclusive");
    assert!(
        matches!(batch.results[0].1, RecordResolution::Present(_)),
        "unexpected lookup results: {:?}",
        batch.results
    );
    assert!(matches!(
        batch.results[1].1,
        RecordResolution::Deleted { .. }
    ));
    assert!(matches!(batch.results[2].1, RecordResolution::Unknown));
}

#[tokio::test]
async fn targeted_record_lookup_present_sibling_keeps_shared_master_state() {
    let response = json!({
        "records": [
            test_master_record("master-shared"),
            {
                "recordName": "asset-a-deleted",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            },
            test_asset_record_for("asset-b-present", "master-shared")
        ]
    });
    let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

    let batch = album
        .resolve_records(&[
            lookup_request("master-shared", "master-shared", "asset-a-deleted"),
            lookup_request("master-shared", "master-shared", "asset-b-present"),
        ])
        .await;

    assert!(batch.complete);
    assert_eq!(batch.results.len(), 1);
    assert!(matches!(batch.results[0].1, RecordResolution::Present(_)));
}

#[tokio::test]
async fn targeted_record_lookup_omitted_sibling_keeps_shared_master_state_unknown() {
    let response = json!({
        "records": [
            test_master_record("master-shared"),
            {
                "recordName": "asset-a-deleted",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            }
        ]
    });
    let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

    let batch = album
        .resolve_records(&[
            lookup_request("master-shared", "master-shared", "asset-a-deleted"),
            lookup_request("master-shared", "master-shared", "asset-b-omitted"),
        ])
        .await;

    assert!(!batch.complete);
    assert_eq!(batch.results.len(), 1);
    assert!(matches!(batch.results[0].1, RecordResolution::Unknown));
}

#[tokio::test]
async fn targeted_record_lookup_retains_transient_failure() {
    let mut session = MockPhotosSession::new();
    for _ in 0..8 {
        session = session.err("temporary lookup failure");
    }
    let album = make_album_with_session(100, Box::new(session));

    let batch = album
        .resolve_records(&[lookup_request("master", "master", "asset")])
        .await;

    assert!(!batch.complete);
    assert!(matches!(
        batch.results[0].1,
        RecordResolution::TransientFailure(ProviderLookupError::Request(_))
    ));
}

#[test]
fn targeted_record_lookup_preserves_typed_http_failures() {
    let error: anyhow::Error = crate::icloud::photos::session::HttpStatusError {
        status: 429,
        url: "https://example.com/records/lookup".to_string(),
        retry_after: None,
        body: None,
    }
    .into();
    assert!(matches!(
        classify_provider_lookup_error(&error),
        ProviderLookupError::RateLimited { status: 429, .. }
    ));

    let error: anyhow::Error = crate::icloud::photos::session::HttpStatusError {
        status: 421,
        url: "https://example.com/records/lookup".to_string(),
        retry_after: None,
        body: None,
    }
    .into();
    assert!(matches!(
        classify_provider_lookup_error(&error),
        ProviderLookupError::Authentication { status: 421, .. }
    ));
}
