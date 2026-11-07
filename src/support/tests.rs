use super::privacy;
use serde_json::json;

const CONCURRENCY_CASES: &[(&str, Option<u16>)] = &[
    ("", Some(10)),
    ("threads=1", Some(1)),
    ("threads=64", Some(64)),
    ("threads=7", Some(7)),
    ("bandwidth_limit='2M'", Some(1)),
    ("bandwidth_limit='0.1K'", Some(1)),
    ("threads=7\nbandwidth_limit='2M'", Some(7)),
    ("threads=64\nbandwidth_limit='1.5Mi'", Some(64)),
    ("threads=0", None),
    ("threads=65", None),
    ("threads=65535", None),
    ("bandwidth_limit='PRIVATE_CONCURRENCY_CANARY'", None),
    ("bandwidth_limit='0'", None),
    (
        "threads=7\nbandwidth_limit='PRIVATE_CONCURRENCY_CANARY'",
        None,
    ),
    (
        "threads=0\nbandwidth_limit='PRIVATE_CONCURRENCY_CANARY'",
        None,
    ),
    ("threads=65536", None),
    ("threads=-1", None),
    ("threads='PRIVATE_CONCURRENCY_CANARY'", None),
];

fn concurrency_fixture() -> tempfile::TempDir {
    let mut root = tempfile::tempdir().unwrap();
    if std::env::var_os("KEI_SNAPSHOT_CONCURRENCY_RETAIN_FIXTURES").is_some() {
        root.disable_cleanup(true);
    }
    eprintln!(
        "SNAPSHOT_CONCURRENCY_FIXTURE root={}",
        root.path().display()
    );
    root
}

#[test]
fn snapshot_concurrency_matches_runtime_configuration() {
    let root = concurrency_fixture();
    let globals = crate::config::GlobalArgs {
        username: Some("synthetic@example.invalid".into()),
        domain: None,
        data_dir: Some(root.path().to_str().unwrap().into()),
    };
    let runtime = |toml: Option<&crate::config::TomlConfig>| {
        crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            crate::cli::SyncArgs::default(),
            toml,
        )
    };
    assert_eq!(super::configuration(Ok(None), true)["threads"], 10);
    assert_eq!(runtime(None).unwrap().download.threads_num, 10);
    assert!(super::configuration(Err(()), true)["threads"].is_null());
    for (index, &(settings, expected)) in CONCURRENCY_CASES.iter().enumerate() {
        let body = format!("[download]\n{settings}");
        let Ok(parsed) = toml::from_str::<crate::config::TomlConfig>(&body) else {
            assert!(expected.is_none());
            assert!(super::configuration(Err(()), true)["threads"].is_null());
            eprintln!("SNAPSHOT_CONCURRENCY_RUNTIME case={index} input=unparseable threads=null");
            continue;
        };
        let projected = super::configuration(Ok(Some(&parsed)), true);
        assert_eq!(projected["threads"], json!(expected), "case {index}");
        match expected {
            Some(count) => assert_eq!(runtime(Some(&parsed)).unwrap().download.threads_num, count),
            None => {
                let error = runtime(Some(&parsed)).unwrap_err().to_string();
                if settings.contains("bandwidth_limit") {
                    assert!(error.starts_with("Invalid [download].bandwidth_limit in config:"));
                } else {
                    let count = parsed.download.as_ref().unwrap().threads.unwrap();
                    assert_eq!(
                        error,
                        format!("[download].threads must be between 1 and 64, got {count}.")
                    );
                }
            }
        }
        eprintln!(
            "SNAPSHOT_CONCURRENCY_RUNTIME case={index} threads={}",
            projected["threads"]
        );
    }
}

#[test]
fn snapshot_concurrency_collection_preserves_inputs_and_output_contract() {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn inputs(root: &Path, output: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut entries = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.path() == output {
                    continue;
                }
                let directory = entry.file_type().unwrap().is_dir();
                let bytes = (!directory).then(|| std::fs::read(entry.path()).unwrap());
                entries.insert(entry.path(), bytes);
                if directory {
                    pending.push(entry.path());
                }
            }
        }
        entries
    }

    for (index, &(settings, expected)) in CONCURRENCY_CASES.iter().enumerate() {
        let root = concurrency_fixture();
        let data = root.path().join("data");
        let media = root.path().join("media");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(&media).unwrap();
        std::fs::write(
            media.join("PRIVATE_CONCURRENCY_CANARY.jpg"),
            b"original media",
        )
        .unwrap();
        let db = rusqlite::Connection::open(data.join("input-state.db")).unwrap();
        db.execute_batch("CREATE TABLE fixture(value TEXT); INSERT INTO fixture VALUES('PRIVATE_CONCURRENCY_CANARY');").unwrap();
        drop(db);
        std::fs::write(
            data.join("health.json"),
            br#"{"total_syncs":3,"private":"PRIVATE_CONCURRENCY_CANARY"}"#,
        )
        .unwrap();
        let marker = root.path().join("password-command-ran");
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "data_dir={}\n[auth]\npassword_command={}\n[download]\ndirectory={}\n{settings}\n",
                json!(data.to_str().unwrap()),
                json!(format!("touch {}", marker.display())),
                json!(media.to_str().unwrap())
            ),
        )
        .unwrap();
        let output = root.path().join("support.json");
        let before = inputs(root.path(), &output);
        eprintln!(
            "SNAPSHOT_CONCURRENCY_COLLECT root={} case={index}",
            root.path().display()
        );
        assert!(super::collect::export(&config, true, &output).unwrap());
        let bytes = std::fs::read(&output).unwrap();
        let bundle: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(bundle["schema_version"], 1);
        assert_eq!(bundle["privacy_contract"], "allowlist_v1");
        assert_eq!(bundle["summary"]["collection"], "offline_read_only");
        assert_eq!(bundle["summary"]["automatic_upload"], false);
        assert!(
            bundle["configuration"]["values"]
                .as_object()
                .unwrap()
                .contains_key("threads")
        );
        assert_eq!(
            bundle["configuration"]["values"]["threads"],
            json!(expected)
        );
        assert_eq!(bundle["state"]["status"], "account_unavailable");
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains("PRIVATE_CONCURRENCY_CANARY"));
        assert!(!text.contains(root.path().to_str().unwrap()));
        assert!(!marker.exists());
        assert_eq!(inputs(root.path(), &output), before);
        assert!(super::collect::export(&config, true, &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        assert_eq!(inputs(root.path(), &output), before);
        assert!(
            super::collect::export(&config, true, &root.path().join("absent/output.json")).is_err()
        );
        assert!(!root.path().join("absent").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let link = root.path().join("output-link.json");
            std::os::unix::fs::symlink(&config, &link).unwrap();
            let original = std::fs::read(&config).unwrap();
            assert!(super::collect::export(&config, true, &link).is_err());
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(std::fs::read(&config).unwrap(), original);
        }
        eprintln!(
            "SNAPSHOT_CONCURRENCY_COLLECT_COMPLETE root={} case={index} threads={}",
            root.path().display(),
            bundle["configuration"]["values"]["threads"]
        );
    }
}

#[test]
fn snapshot_concurrency_distinguishes_missing_from_unavailable_configuration() {
    for kind in [
        "missing",
        "empty",
        "invalid",
        "directory",
        "dangling-symlink",
    ] {
        #[cfg(not(unix))]
        if kind == "dangling-symlink" {
            continue;
        }
        let root = concurrency_fixture();
        let config = root.path().join("config.toml");
        match kind {
            "missing" => {}
            "empty" => std::fs::write(&config, "").unwrap(),
            "invalid" => std::fs::write(&config, "[download]\nthreads = [").unwrap(),
            "directory" => std::fs::create_dir(&config).unwrap(),
            "dangling-symlink" => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(root.path().join("absent.toml"), &config).unwrap();
            }
            _ => panic!("unknown availability fixture: {kind}"),
        }
        let output = root.path().join("support.json");
        assert!(super::collect::export(&config, true, &output).unwrap());
        let bundle: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        let expected = matches!(kind, "missing" | "empty").then_some(10);
        assert_eq!(
            bundle["configuration"]["values"]["threads"],
            json!(expected)
        );
        assert_eq!(bundle["state"]["status"], "account_unavailable");
        eprintln!(
            "SNAPSHOT_CONCURRENCY_AVAILABILITY root={} kind={kind} threads={}",
            root.path().display(),
            bundle["configuration"]["values"]["threads"]
        );
    }
}

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
    let configuration = privacy::configuration(&malicious);
    assert_eq!(configuration["threads"], 2);
    assert!(!configuration.to_string().contains("private"));
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
}

#[test]
fn existing_refresh_hold_and_hardlink_fixed_fields_survive_production_layer() {
    use tracing_subscriber::layer::SubscriberExt;
    let (layer, drain) = super::test_capture::test_layer();
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
    for (label, expected) in [
        ("name-size-dedup-with-suffix", "name-size-dedup-with-suffix"),
        ("name-id7", "name-id7"),
    ] {
        let input: crate::config::TomlConfig =
            toml::from_str(&format!("[photos]\nfile_match_policy={label:?}")).unwrap();
        assert_eq!(
            super::configuration(Ok(Some(&input)), true)["file_match_policy"],
            expected
        );
    }
    for label in ["as-is", "prefer-raw", "prefer-jpeg"] {
        let input: crate::config::TomlConfig =
            toml::from_str(&format!("[photos]\nraw_policy={label:?}")).unwrap();
        assert_eq!(
            super::configuration(Ok(Some(&input)), true)["raw_policy"],
            label
        );
    }
    for label in ["both", "image-only", "video-only", "skip"] {
        let input: crate::config::TomlConfig =
            toml::from_str(&format!("[photos]\nlive_photo_mode={label:?}")).unwrap();
        assert_eq!(
            super::configuration(Ok(Some(&input)), true)["live_photo_mode"],
            label
        );
    }
    let input: crate::config::TomlConfig =
        toml::from_str("[auth]\nusername=\"PRIVATE_USER\"").unwrap();
    assert!(
        !super::configuration(Ok(Some(&input)), true)
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
    // Older diagnostic records carry no newly invented response evidence.
    let fields = json!({"target":"paired","reason":"record_provider_error"});
    assert_eq!(
        privacy::diagnostic("exact_lookup_rejection_v1", fields.as_object().unwrap()).unwrap(),
        fields.as_object().unwrap().clone()
    );
}

#[test]
fn issue956_child_reference_status_is_a_field_specific_allowlist() {
    for label in [
        "not_requested",
        "omitted",
        "unavailable",
        "invalid_child",
        "missing",
        "malformed",
        "sparse",
        "matches_requested",
        "different",
    ] {
        let fields = json!({"child_reference_status":label});
        assert_eq!(
            privacy::diagnostic("exact_lookup_rejection_v1", fields.as_object().unwrap()).unwrap(),
            *fields.as_object().unwrap()
        );
    }
    for invalid in [
        json!("PRIVATE_REFERENCE_CANARY"),
        json!("returned"),
        json!(42),
        json!(null),
        json!(true),
        json!({"recordName":"PRIVATE"}),
    ] {
        let fields = json!({"child_reference_status":invalid,"reason":"record_provider_error"});
        let clean =
            privacy::diagnostic("exact_lookup_rejection_v1", fields.as_object().unwrap()).unwrap();
        assert_eq!(
            clean,
            *json!({"reason":"record_provider_error"})
                .as_object()
                .unwrap()
        );
    }
    let old = json!({"target":"paired","reason":"record_provider_error"});
    let clean = privacy::diagnostic("exact_lookup_rejection_v1", old.as_object().unwrap()).unwrap();
    assert_eq!(
        clean,
        *old.as_object().unwrap(),
        "old diagnostic records cannot invent reference evidence"
    );
}

#[test]
fn recovery_decisions_preserve_only_fixed_evidence_outcomes() {
    for (decision, reason) in [
        ("partial", "historical_claim_prepared"),
        ("unsupported", "historical_claim_unverified"),
    ] {
        let fields = json!({
            "mechanism": "historical_path", "decision": decision, "reason": reason,
            "owner": "private-owner", "path": "/private/media", "record": "private-record",
            "token": "private-token", "url": "https://private-provider"
        });
        let projected =
            privacy::diagnostic("recovery_decision_v1", fields.as_object().unwrap()).unwrap();
        assert_eq!(
            json!(projected),
            json!({"mechanism":"historical_path","decision":decision,"reason":reason})
        );
    }
    let fields =
        json!({"mechanism":"private-owner","decision":"private-token","reason":"private-path"});
    let projected =
        privacy::diagnostic("recovery_decision_v1", fields.as_object().unwrap()).unwrap();
    assert!(
        projected.is_empty(),
        "untrusted diagnostic fields must not authorize arbitrary labels"
    );
}
