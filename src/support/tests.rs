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
    let fields = super::error_fields("download", &error);
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
            json!({"reason":"invalid_inventory_evidence","phase":"family_scan","subreason":"child_reference_scope_mismatch","eof_observed":true,"family_context":"unrelated","scope_mismatch_component":"owner","pages":5,"records":604,"transferred_bytes":40000000,"retained_bytes":25000000,"retained_byte_budget":268435456,"elapsed_secs":1020.0}),
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
