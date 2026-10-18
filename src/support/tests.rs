use super::privacy;
use serde_json::json;

#[test]
fn malicious_reports_configurations_and_errors_are_allowlisted() {
    let malicious = json!({
        "username": "private-apple-id", "password": "private-password", "token": "private-token", "cookie": "private-cookie",
        "download_dir": "/private/path", "albums": ["private-album"], "library": "private-zone", "checksum": "private-checksum",
        "resolution": "private-provider-response", "inventory_drop_library": "private-library", "sync_token_blocked_zone": "private-zone",
        "sync_token_blocked_explanation": "private-URL", "sync_token_blocked_reason": "private-error",
        "bytes_downloaded": 123, "disk_bytes_written": 456, "threads": 2,
        "primary_layout": {"bound_families": 4, "private": "private-id"},
    });
    let stats = privacy::stats(&malicious);
    let configuration = privacy::configuration(&malicious);
    assert_eq!(stats["bytes_downloaded"], 123);
    assert_eq!(stats["disk_bytes_written"], 456);
    assert_eq!(configuration["threads"], 2);
    assert!(!format!("{stats}{configuration}").contains("private"));
    let error = anyhow::anyhow!(
        "https://private-url /private-path password=private-token AppleID=private-apple-id"
    );
    let fields = super::classify_support_error("download", &error);
    assert_eq!(fields["class"], "other");
    assert!(!fields.to_string().contains("private"));
}

#[test]
fn historical_support_questions_have_safe_structured_answers() {
    // Each fixture states the actual historical question and concrete safe
    // answer fields, rather than equating a counter collection with diagnosis.
    let cases = [
        (
            "765 external restart",
            "support_startup_v1",
            json!({"phase":"shutdown","outcome":"interrupted","interrupted":true}),
        ),
        (
            "853 config provenance",
            "support_startup_v1",
            json!({"phase":"configuration","outcome":"ready"}),
        ),
        (
            "770 query completion",
            "support_pass_completion_v1",
            json!({"completion":"proven_eof","selected_scope":"selected","selection_comparable":false}),
        ),
        (
            "765 receiver mismatch",
            "support_pass_completion_v1",
            json!({"expected_fetchers":4,"completed_fetchers":4,"token_count":4,"unique_token_count":2}),
        ),
        (
            "924 bridge successor",
            "support_checkpoint_v1",
            json!({"bridge":true,"successor_provenance":"provider","token_present":true,"reason":"veto"}),
        ),
        (
            "925 inventory comparability",
            "support_pass_completion_v1",
            json!({"api_total":604,"assets_seen":17,"inventory_complete":false,"selection_comparable":false}),
        ),
        ("765 sparse debt", "sparse_identity_state_failed", json!({})),
        (
            "845 owner morphology",
            "exact_lookup_rejection_v1",
            json!({"target":"paired","stage":"master_reference_scope","reason":"owner_unqualified","expected_owner":"absent","lookup_zone":"primary","rejected_requests":604,"supplied_owner":"other_string","supplied_owner_type":"string","owner_provenance":"lookup_request"}),
        ),
        (
            "853 ambiguity",
            "metadata_capture_ambiguity_counts_v1",
            json!({"stored_renditions":2,"matching_children":3,"full_evidence_matching_children":1}),
        ),
        (
            "845 inventory unrelated scope",
            "legacy_inventory_failure_v2",
            json!({"reason":"invalid_inventory_evidence","phase":"family_scan","subreason":"child_reference_scope_mismatch","eof_observed":true,"family_context":"unrelated","scope_mismatch_component":"owner","expected_owner":"absent","expected_owner_type":"absent","supplied_owner":"private_default","supplied_owner_type":"string","pages":5,"records":604,"transferred_bytes":40000000,"retained_bytes":25000000,"retained_byte_budget":268435456,"elapsed_secs":1020.0}),
        ),
        (
            "845 refresh omissions",
            "exact_refresh_outcomes_v1",
            json!({"requested_task_keys":604,"refreshed_task_keys":17,"remaining_task_keys":587,"phase_elapsed_secs":4.0,"oldest_refreshed_url_observed_age_secs":8.0}),
        ),
        (
            "845 receipt reuse",
            "support_publication_v1",
            json!({"operation":"receipt_recovery","receipt_match":true,"content_equal":true,"size_equal":true,"publication_time_retained":true,"actual_publication":false,"source_status":"pending","destination_status":"downloaded"}),
        ),
        (
            "858 filesystem error",
            "support_task_error_v1",
            json!({"operation":"publication","class":"io","errno":95}),
        ),
        (
            "914 first attempt before eventual success",
            "support_transfer_pass_v1",
            json!({"phase":"first","failed":17,"downloaded":604,"concurrency":2,"state_write_failures":0}),
        ),
        (
            "913 strict import",
            "support_import_v1",
            json!({"phase":"adoption","reason":"strict_refusal","dry_run":true}),
        ),
        (
            "929 precision",
            "support_metadata_v1",
            json!({"operation":"embed","backend":"native_exif","source_fractional":true,"planned_fractional":true,"written_fractional":false,"verified":true}),
        ),
        (
            "926 provider readiness",
            "support_startup_v1",
            json!({"phase":"startup","class":"provider_not_ready","http_status":null}),
        ),
    ];
    for (question, kind, fields) in cases {
        let clean = privacy::diagnostic(kind, fields.as_object().unwrap()).unwrap();
        assert_eq!(
            serde_json::Value::Object(clean),
            fields,
            "missing actionable evidence for {question}"
        );
    }
}

#[test]
fn tim_paired_counts_remain_overlapping_observations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.json");
    // Export never sums 604/17/604 into a unique asset count.
    let mut cycle = json!({"id":uuid::Uuid::new_v4().to_string(),"started_at":"2026-10-10T16:00:00Z","completed_at":null,
        "outcome":"running","build_version":"0.25.1-dev","configuration":{},"stats":{},"diagnostics":[]});
    cycle["diagnostics"] = json!([604,17,604].into_iter().map(|n| json!({"kind":"exact_lookup_rejection_v1",
        "fields":{"target":"paired","stage":"master_reference_scope","reason":"owner_unqualified","rejected_requests":n},
        "observations":1,"first_at":"2026-10-10T16:01:00Z","last_at":"2026-10-10T16:01:00Z"})).collect::<Vec<_>>());
    std::fs::write(&path, serde_json::to_vec(&json!({"schema_version":1,"cycles_evicted":0,"queue_dropped":0,"groups_omitted":0,"previous_history_unavailable":false,"cycles":[cycle]})).unwrap()).unwrap();
    let (h, status) = super::history::read(&path);
    assert_eq!(status, "available");
    assert_eq!(h.cycles[0].diagnostics.len(), 3);
    assert_eq!(h.cycles[0].diagnostics[1].fields["rejected_requests"], 17);
}

#[test]
fn owning_checkpoint_decisions_survive_support_allowlist_without_tokens() {
    use crate::download::RecoveryAction;
    use crate::sync_cycle::{
        CheckpointBasis, CheckpointHoldReason, SourceCheckpointDecision, checkpoint_support_fields,
    };
    for reason in [
        CheckpointHoldReason::DryRun,
        CheckpointHoldReason::StalePassPlan,
        CheckpointHoldReason::Interrupted,
        CheckpointHoldReason::SessionExpired,
        CheckpointHoldReason::EnumerationIncomplete,
        CheckpointHoldReason::StateNotDurable,
        CheckpointHoldReason::TokenProofIncomplete,
        CheckpointHoldReason::LegacyPreservationIncomplete,
    ] {
        let decision = SourceCheckpointDecision::Preserve {
            reason,
            recovery: RecoveryAction::ReplayFromPriorToken,
        };
        let fields = checkpoint_support_fields(&decision);
        let projected =
            super::privacy::diagnostic("support_checkpoint_v1", fields.as_object().unwrap())
                .unwrap();
        assert_eq!(serde_json::Value::Object(projected), fields);
        assert_eq!(fields["decision"], "preserved");
    }
    for basis in [
        CheckpointBasis::IncrementalDelta,
        CheckpointBasis::CompleteInventory,
        CheckpointBasis::InventoryWithDeltaBridge,
    ] {
        let fields = checkpoint_support_fields(&SourceCheckpointDecision::Advance {
            token: "PRIVATE_PROVIDER_TOKEN".into(),
            basis,
        });
        let projected =
            super::privacy::diagnostic("support_checkpoint_v1", fields.as_object().unwrap())
                .unwrap();
        assert_eq!(serde_json::Value::Object(projected), fields);
        assert!(!fields.to_string().contains("PRIVATE"));
    }
    for persistence in [
        "failed",
        "stored",
        "stored_hold",
        "staged_reconciliation",
        "no_state_db",
    ] {
        let fields = serde_json::json!({"persistence": persistence});
        assert_eq!(
            serde_json::Value::Object(
                super::privacy::diagnostic("support_checkpoint_v1", fields.as_object().unwrap())
                    .unwrap()
            ),
            fields
        );
    }
    assert_eq!(
        super::privacy::stats(
            &serde_json::json!({"sync_token_blocked_reason":"inventory_delta_bridge_failed"})
        )["sync_token_blocked_reason"],
        "inventory_delta_bridge_failed"
    );
}

#[test]
fn existing_refresh_hold_and_hardlink_fixed_fields_survive_production_layer() {
    use tracing_subscriber::layer::SubscriberExt;
    let (layer, drain) = super::history::test_layer();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        tracing::warn!(
            diagnostic = "expired_url_refresh_failed",
            failed_records = 17u64,
            authentication_failures = 2u64,
            rate_limit_observations = 3u64,
            error = "PRIVATE_TOKEN"
        );
        tracing::warn!(
            diagnostic = "retained_checkpoint_expired",
            retry_deferred = true,
            state_write_failed = false,
            retry_exhausted = true,
            zone = "PRIVATE_ZONE"
        );
        tracing::warn!(
            diagnostic = "legacy_preservation_hardlink_trust",
            affected_paths = 4u64
        );
    });
    let evidence = drain();
    assert_eq!(evidence.len(), 3);
    assert_eq!(
        evidence[0]["fields"],
        json!({"failed_records":17,"authentication_failures":2,"rate_limit_observations":3})
    );
    assert_eq!(
        evidence[1]["fields"],
        json!({"retry_deferred":true,"state_write_failed":false,"retry_exhausted":true})
    );
    assert_eq!(evidence[2]["fields"], json!({"affected_paths":4}));
    assert!(
        !serde_json::to_string(&evidence)
            .unwrap()
            .contains("PRIVATE")
    );
}

#[test]
fn typed_effective_configuration_preserves_every_file_raw_and_live_mode() {
    use crate::types::{FileMatchPolicy, LivePhotoMode, RawPolicy};
    let dir = tempfile::tempdir().unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some("PRIVATE_USER".into()),
        domain: None,
        data_dir: Some(dir.path().to_string_lossy().into_owned()),
    };
    let toml: crate::config::TomlConfig = toml::from_str(&format!(
        "[download]\ndirectory={:?}",
        dir.path().to_string_lossy()
    ))
    .unwrap();
    let mut config = crate::config::Config::build(
        &globals,
        &crate::cli::PasswordArgs::default(),
        crate::cli::SyncArgs::default(),
        Some(&toml),
    )
    .unwrap();
    for (variant, label) in [
        (
            FileMatchPolicy::NameSizeDedupWithSuffix,
            "name-size-dedup-with-suffix",
        ),
        (FileMatchPolicy::NameId7, "name-id7"),
    ] {
        config.photos.file_match_policy = variant;
        assert_eq!(
            super::runtime_configuration(&config)["file_match_policy"],
            label
        );
    }
    for (variant, label) in [
        (RawPolicy::AsIs, "as-is"),
        (RawPolicy::PreferRaw, "prefer-raw"),
        (RawPolicy::PreferJpeg, "prefer-jpeg"),
    ] {
        config.photos.raw_policy = variant;
        assert_eq!(super::runtime_configuration(&config)["raw_policy"], label);
    }
    for (variant, label) in [
        (LivePhotoMode::Both, "both"),
        (LivePhotoMode::ImageOnly, "image-only"),
        (LivePhotoMode::VideoOnly, "video-only"),
        (LivePhotoMode::Skip, "skip"),
    ] {
        config.photos.live_photo_mode = variant;
        assert_eq!(
            super::runtime_configuration(&config)["live_photo_mode"],
            label
        );
        let input: crate::config::TomlConfig =
            toml::from_str(&format!("[photos]\nlive_photo_mode={label:?}")).unwrap();
        assert_eq!(
            super::configuration(Some(&input), true)["live_photo_mode"],
            label
        );
    }
    assert!(
        !super::runtime_configuration(&config)
            .to_string()
            .contains("PRIVATE")
    );
}

#[test]
fn hydration_wait_contract_retains_only_fixed_phases_outcomes_and_unsigned_time() {
    for phase in ["asset_identity", "paired_asset", "album_relation"] {
        for outcome in ["started", "waiting", "returned", "cancelled"] {
            let fields = json!({"phase": phase, "outcome": outcome, "elapsed_secs": 31,
                "token": "private-token", "library": "private-zone", "message": "private-body"});
            let safe =
                privacy::diagnostic("support_hydration_wait_v1", fields.as_object().unwrap())
                    .unwrap();
            assert_eq!(
                serde_json::Value::Object(safe),
                json!({"phase": phase, "outcome": outcome, "elapsed_secs": 31})
            );
        }
    }
    let fields =
        json!({"phase": "private-zone", "outcome": "private-response", "elapsed_secs": -1});
    assert!(
        privacy::diagnostic("support_hydration_wait_v1", fields.as_object().unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn missing_reference_contract_rejects_private_and_numeric_categories() {
    let expected = json!({"record_history":"replacement", "prior_reference":"present",
        "sparse_marker":"true", "linkage":"all_fields", "lookup_outcome":"live_reference", "lookup_family":"candidate", "lookup_scope":"matched", "lookup_shape":"live_reference"});
    assert_eq!(
        serde_json::Value::Object(
            privacy::diagnostic(
                "support_missing_reference_v1",
                expected.as_object().unwrap()
            )
            .unwrap()
        ),
        expected
    );
    for value in [
        json!(42),
        json!("private-provider-value"),
        json!({"nested":"private"}),
        json!(null),
    ] {
        let fields = expected
            .as_object()
            .unwrap()
            .keys()
            .map(|key| (key.clone(), value.clone()))
            .collect();
        assert!(
            privacy::diagnostic("support_missing_reference_v1", &fields)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn issue956_lookup_record_status_contract_is_closed_and_optional() {
    for label in [
        "not_requested",
        "omitted",
        "returned",
        "tombstone",
        "not_found",
        "access_denied",
        "authentication_failed",
        "zone_not_found",
        "retry_later",
        "throttled",
        "server_error",
        "other_error",
        "malformed_error",
    ] {
        let fields = json!({"master_status":label,"child_status":label});
        assert_eq!(
            privacy::diagnostic("exact_lookup_rejection_v1", fields.as_object().unwrap()).unwrap(),
            fields.as_object().unwrap().clone()
        );
    }
    for invalid in [
        json!("ACCESS_DENIED"),
        json!("PRIVATE_ERROR_CANARY"),
        json!(401),
        json!(null),
        json!({"secret":"PRIVATE"}),
    ] {
        let fields = json!({"master_status":invalid,"child_status":invalid,"reason":"record_provider_error"});
        let clean =
            privacy::diagnostic("exact_lookup_rejection_v1", fields.as_object().unwrap()).unwrap();
        assert_eq!(
            clean,
            json!({"reason":"record_provider_error"})
                .as_object()
                .unwrap()
                .clone()
        );
    }
    // Older retained history carries no newly invented response evidence.
    let fields = json!({"target":"paired","reason":"record_provider_error"});
    assert_eq!(
        privacy::diagnostic("exact_lookup_rejection_v1", fields.as_object().unwrap()).unwrap(),
        fields.as_object().unwrap().clone()
    );
}
