//! Export allowlist. Raw input, log messages and error text never enter the wire format.
use serde_json::{Map, Value};

pub(super) const NUMERIC_STATS: &[&str] = &[
    "assets_seen",
    "api_total_at_start",
    "api_total_at_start_partial",
    "downloaded",
    "failed",
    "bytes_downloaded",
    "disk_bytes_written",
    "exif_failures",
    "metadata_capture_revision",
    "metadata_capture_refreshed",
    "metadata_capture_failures",
    "metadata_capture_remaining",
    "metadata_capture_unresolved",
    "metadata_capture_deferred",
    "unattributed_legacy_assets",
    "unattributed_legacy_pending",
    "state_write_failures",
    "enumeration_errors",
    "count_probe_failures",
    "stale_pending_pruned",
    "pagination_shortfall_warnings",
    "pagination_shortfall_assets",
    "tail_probes",
    "count_undercount_assets",
    "enumeration_incomplete",
    "inventory_drop_warnings",
    "inventory_drop_assets",
    "inventory_drop_percent",
    "inventory_drop_previous_total",
    "inventory_drop_current_total",
    "sync_token_blocked",
    "sync_token_expected_receivers",
    "sync_token_receivers_with_token",
    "sync_token_receivers_missing",
    "sync_token_receivers_blank",
    "sync_token_receivers_dropped",
    "sync_token_unique_values",
    "same_cycle_recovery_attempts",
    "same_cycle_recovery_successes",
    "elapsed_secs",
    "interrupted",
    "rate_limited",
    "photos_downloaded",
    "videos_downloaded",
];

const CONFIG_NUMBERS: &[&str] = &[
    "threads",
    "dry_run",
    "edited",
    "alternative",
    "force_resolution",
    "skip_videos",
    "skip_photos",
    "set_exif_datetime",
    "set_exif_rating",
    "set_exif_gps",
    "set_exif_description",
    "embed_xmp",
    "xmp_sidecar",
    "album_selectors",
    "smart_folder_selectors",
    "library_selectors",
    "filename_exclusions",
    "unfiled",
    "date_filter_present",
    "recent_limit_present",
    "watch_interval_secs",
    "per_transfer",
    "per_asset",
    "legacy_preservation_allow_hardlinks",
    "keep_unicode_in_filenames",
    "report_configured",
    "config_explicit",
    "data_dir_from_environment",
    "username_from_environment",
    "data_dir_configured",
    "password_command_configured",
    "password_file_configured",
    "server_configured",
    "album_exclusions",
    "smart_folder_exclusions",
    "library_exclusions",
    "sensitive_folders",
    "primary_library",
    "shared_libraries",
    "notification_configured",
    "folder_template_custom",
    "album_template_custom",
    "smart_template_custom",
    "progress_bar",
    "reconcile_every_n_cycles",
    "strict",
    "xmp_feature",
];

pub(super) fn scalar(value: &Value) -> bool {
    value.is_number() || value.is_boolean() || value.is_null()
}

pub(super) fn numbers(value: &Value, fields: &[&str]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|key| {
                let value = value.get(*key)?;
                scalar(value).then(|| ((*key).to_owned(), value.clone()))
            })
            .collect(),
    )
}

pub(super) fn stats(value: &Value) -> Value {
    let mut out = numbers(value, NUMERIC_STATS);
    if let Some(map) = out.as_object_mut() {
        for key in [
            "sync_token_blocked_reason",
            "sync_token_blocked_source",
            "full_enumeration_reason",
        ] {
            if let Some(v) = value
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| fixed_label(v))
            {
                map.insert(key.to_owned(), Value::String(v.to_owned()));
            }
        }
        if let Some(v) = value.get("skipped") {
            map.insert(
                "skipped".into(),
                numbers(
                    v,
                    &[
                        "on_disk",
                        "by_media_type",
                        "by_date_range",
                        "by_live_photo",
                        "by_filename",
                        "by_excluded_album",
                        "ampm_variant",
                        "retry_only",
                        "by_state",
                        "by_filter",
                        "duplicates",
                        "retry_exhausted",
                        "by_existing",
                        "by_policy",
                        "filtered",
                        "already_downloaded",
                        "filename",
                        "date",
                        "media",
                        "size",
                        "live_photo",
                    ],
                ),
            );
        }
        if let Some(v) = value.get("primary_layout").filter(|v| !v.is_null()) {
            map.insert(
                "primary_layout".into(),
                numbers(
                    v,
                    &[
                        "active_revision",
                        "pending_revision",
                        "managed_assets",
                        "legacy_assets",
                        "managed_files",
                        "legacy_files",
                        "pending_operations",
                        "held_operations",
                        "bound_families",
                        "preserved_files",
                    ],
                ),
            );
        }
    }
    out
}

pub(super) fn configuration(value: &Value) -> Value {
    let mut out = numbers(value, CONFIG_NUMBERS);
    if let Some(map) = out.as_object_mut() {
        for key in [
            "album_mode",
            "smart_folder_mode",
            "resolution",
            "live_resolution",
            "live_photo_mode",
            "edited_naming",
            "raw_policy",
            "file_match_policy",
            "live_photo_mov_filename_policy",
            "recent_scope",
        ] {
            if let Some(v) = value
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| fixed_label(v))
            {
                map.insert(key.into(), Value::String(v.into()));
            }
        }
        if let Some(media) = value.get("media").and_then(Value::as_array) {
            map.insert(
                "media".into(),
                Value::Array(
                    media
                        .iter()
                        .filter(|v| {
                            matches!(
                                v.as_str(),
                                Some("photos" | "videos" | "live_photos" | "live-photos")
                            )
                        })
                        .take(3)
                        .cloned()
                        .collect(),
                ),
            );
        }
    }
    out
}

// Every diagnostic has an explicit field contract. Adding a tracing field alone
// cannot broaden the export. Counts are observations, never unique asset counts.
pub(super) fn contract(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "exact_lookup_rejection_v1" => Some(&[
            "target",
            "stage",
            "reason",
            "expected_owner",
            "lookup_zone",
            "rejected_requests",
            "supplied_owner",
            "supplied_owner_type",
            "owner_provenance",
        ]),
        "legacy_inventory_failure_v2" => Some(&[
            "stage",
            "reason",
            "phase",
            "subreason",
            "eof_observed",
            "family_context",
            "child_soft_deleted",
            "pages",
            "records",
            "transferred_bytes",
            "retained_bytes",
            "scope_mismatch_component",
            "elapsed_secs",
            "page_budget",
            "record_budget",
            "page_byte_budget",
            "retained_byte_budget",
        ]),
        "metadata_capture_ambiguity_counts_v1" => Some(&[
            "stored_renditions",
            "matching_children",
            "full_evidence_matching_children",
        ]),
        "exact_refresh_child_rejection_v1" => Some(&["reason", "rejected_child_requests"]),
        "exact_refresh_task_rejection_v1" => Some(&["reason", "rejected_task_keys"]),
        "exact_refresh_outcomes_v1" => Some(&[
            "requested_task_keys",
            "planned_unique_child_requests",
            "child_master_references",
            "child_lookup_results",
            "planned_paired_requests",
            "paired_lookup_results",
            "present_pairs",
            "refreshed_task_keys",
            "remaining_task_keys",
            "cancellation_observed",
            "authentication_failed",
            "phase_elapsed_secs",
            "oldest_refreshed_url_observed_age_secs",
            "rate_limit_observations",
        ]),
        "record_omitted"
        | "record_not_found"
        | "record_access_denied"
        | "record_zone_not_found"
        | "record_provider_error"
        | "unexpected_record_type"
        | "record_decode_failed"
        | "master_reference_missing"
        | "master_reference_malformed"
        | "master_reference_present"
        | "sparse_share_reference_unresolved"
        | "sparse_share_reference_malformed" => Some(&["reference_zone", "lookup_zone", "count"]),
        "support_task_error_v1" => Some(&[
            "stage",
            "publication_committed",
            "operation",
            "class",
            "errno",
            "http_status",
            "sqlite_code",
            "metadata_requested",
        ]),
        "support_transfer_pass_v1" => Some(&[
            "phase",
            "failed",
            "downloaded",
            "auth_errors",
            "state_write_failures",
            "exif_failures",
            "concurrency",
            "url_expired",
            "interrupted",
            "elapsed_secs",
        ]),
        "support_discovery_v1" => Some(&[
            "phase",
            "database",
            "owner",
            "selected_indexing",
            "qualification_published",
        ]),
        "support_publication_v1" => Some(&[
            "transaction_committed",
            "reason",
            "operation",
            "receipt_match",
            "content_equal",
            "size_equal",
            "publication_time_retained",
            "companion_stem_equal",
            "metadata_requested",
            "actual_publication",
            "source_status",
            "destination_status",
            "updated",
        ]),
        "support_import_v1" => Some(&[
            "phase",
            "reason",
            "total",
            "matched",
            "unmatched",
            "filtered",
            "strict_refused",
            "strict",
            "recent_limit_present",
            "hash_errors",
            "skipped_already_imported",
            "dry_run",
            "legacy_identity",
            "master_child_match",
            "path_shape_equal",
        ]),
        "support_metadata_v1" => Some(&[
            "timestamp_planned",
            "operation",
            "backend",
            "source_fractional",
            "planned_fractional",
            "written_fractional",
            "verified",
            "applied",
        ]),
        "support_pass_completion_v1" => Some(&[
            "phase",
            "completion",
            "expected_fetchers",
            "completed_fetchers",
            "token_present",
            "token_count",
            "unique_token_count",
            "suppressed",
            "selected_scope",
            "inventory_complete",
            "selection_comparable",
            "api_total",
            "assets_seen",
        ]),
        "support_checkpoint_v1" => Some(&[
            "decision",
            "basis",
            "persistence",
            "reason",
            "recovery",
            "token_present",
            "successor_provenance",
            "bridge",
            "full_enumeration",
            "identity_incomplete",
            "state_write_failures",
            "enumeration_errors",
        ]),
        "support_startup_v1" => Some(&[
            "phase",
            "outcome",
            "class",
            "errno",
            "http_status",
            "interrupted",
        ]),
        "legacy_preservation_hardlink_trust" => Some(&["stage", "reason", "affected_paths"]),
        "expired_url_refresh_failed" => Some(&[
            "failed_records",
            "authentication_failures",
            "rate_limit_observations",
        ]),
        "retained_checkpoint_expired" => {
            Some(&["retry_deferred", "state_write_failed", "retry_exhausted"])
        }
        "sparse_identity_state_failed"
        | "sparse_deletion_validation_failed"
        | "sparse_share_reference_changed"
        | "asset_delta_identity_unresolved"
        | "incomplete_record_pair"
        | "retained_checkpoint_hold_invalid"
        | "stale_plan_unaffected_zone"
        | "authentication"
        | "rate_limited"
        | "request_failed"
        | "malformed_response"
        | "pending_retry_unmatched" => Some(&[]),
        _ => None,
    }
}

pub(super) fn diagnostic(kind: &str, fields: &Map<String, Value>) -> Option<Map<String, Value>> {
    let allowed = contract(kind)?;
    Some(
        fields
            .iter()
            .filter(|(k, v)| {
                (allowed.contains(&k.as_str())
                    && (scalar(v) || v.as_str().is_some_and(fixed_label)))
                    || (matches!(k.as_str(), "item_alias" | "scope_alias")
                        && v.as_str().is_some_and(valid_alias))
                    || (k.as_str() == "correlation_unavailable" && v.is_boolean())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

pub(crate) fn fixed_label(value: &str) -> bool {
    LABELS.contains(&value)
}

const LABELS: &[&str] = &[
    "name-size-dedup-with-suffix",
    "name-id7",
    "prefer-raw",
    "prefer-jpeg",
    "image-only",
    "video-only",
    "skip",
    "metadata_probe",
    "metadata_prepare",
    "metadata_publish",
    "sidecar_plan",
    "sidecar_write",
    "source_gps",
    "enumeration_incomplete",
    "dry_run",
    "stale_pass_plan",
    "state_not_durable",
    "token_proof_incomplete",
    "legacy_preservation_incomplete",
    "inventory_delta_bridge_failed",
    "incremental_delta",
    "complete_inventory",
    "inventory_with_delta_bridge",
    "no_state_db",
    "staged_reconciliation",
    "stored",
    "stored_hold",
    "name_only_indexing",
    "selected_qualified_indexing",
    "private",
    "named",
    "probe",
    "unrelated",
    "preparation",
    "certification",
    "checkpoint",
    "receipt_missing",
    "receipt_changed",
    "publication_owner_mismatch",
    "state_transition_mismatch",
    "current",
    "family_scan",
    "response_decode_failed",
    "capture_date_missing",
    "child_master_identity_invalid",
    "child_master_reference_missing",
    "child_reference_scope_mismatch",
    "completion_marker_missing",
    "cursor_missing",
    "cursor_repeated",
    "family_hydration_incomplete",
    "invalid_zone_envelope",
    "missing_candidate_scope",
    "missing_zone_scope",
    "record_identity_missing",
    "record_provider_error",
    "record_type_missing",
    "records_array_missing",
    "unexpected_child",
    "zone_provider_error",
    "zone_scope_mismatch",
    "invalid_content",
    "provider_not_ready",
    "null",
    "string",
    "number",
    "boolean",
    "object",
    "array",
    "empty",
    "private_default",
    "other_string",
    "lookup_request",
    "zone_name",
    "owner",
    "extra_component",
    "candidate",
    "capture_date",
    "child_reference",
    "cursor",
    "decode",
    "envelope",
    "family_hydration",
    "initialization",
    "not_applicable",
    "pagination",
    "provider_status",
    "record_envelope",
    "request",
    "response_budget",
    "retention",
    "scope",
    "unclassified",
    "unknown",
    "album_delta_state_write_failed",
    "album_relation_hydration_incomplete",
    "asset_delta_hydration_incomplete",
    "asset_master_mapping_state_write_failed",
    "await_retained_checkpoint_evidence",
    "continue_tail",
    "date_bounded_full_enumeration",
    "icloud",
    "icloud_album_count_error",
    "icloud_blank_sync_token",
    "icloud_sync_token_mismatch",
    "icloud_sync_token_missing",
    "incremental_delete_no_matching_state",
    "incremental_delete_state_write_failed",
    "incremental_hidden_no_matching_state",
    "incremental_hidden_state_write_failed",
    "kei",
    "kei_internal_token_receiver_dropped",
    "metadata_capture_repair_failed",
    "pagination_shortfall",
    "pending_retry_unmatched",
    "producer_enumeration_incomplete",
    "provider_metadata_state_write_failed",
    "reauthenticate",
    "recent_limited_full_enumeration",
    "reconcile_inventory",
    "repair_retained_checkpoint_evidence",
    "replay_from_prior_token",
    "retained_checkpoint_expired",
    "retained_checkpoint_hold_invalid",
    "retained_checkpoint_retry_exhausted",
    "retry_passes",
    "revalidate_records",
    "selection_generation_deferred",
    "smart_folder_refresh_failed",
    "stop",
    "sync_token_missing",
    "sync_token_unavailable",
    "targeted_album_backfill_failed",
    "unknown",
    "unknown_album_relation_asset",
    "unknown_album_relation_container",
    "unparsable_relation_delta",
    "absent",
    "malformed",
    "same_zone",
    "different_or_partial_zone",
    "primary",
    "shared",
    "other",
    "private_default",
    "child",
    "master",
    "paired",
    "record_scope",
    "master_reference_scope",
    "pairing",
    "record_error",
    "record_presence",
    "record_decode_or_reference",
    "malformed_scope",
    "partial_scope",
    "zone_conflict",
    "owner_unqualified",
    "owner_conflict",
    "scope_component_conflict",
    "child_master_mismatch",
    "record_provider_error",
    "child_record_omitted",
    "master_record_omitted",
    "record_identity_incomplete",
    "invalid_inventory_evidence",
    "cancelled",
    "provider_request_failed",
    "page_budget",
    "record_budget",
    "response_page_byte_budget",
    "retained_byte_budget",
    "initialization",
    "unclassified",
    "not_applicable",
    "requested_family",
    "unrelated_record",
    "inventory",
    "scan",
    "decode",
    "pair",
    "validation",
    "query",
    "unrelated_child",
    "requested_child",
    "requested_master",
    "soft_deleted",
    "visible",
    "hidden",
    "candidate",
    "prepare",
    "commit",
    "metadata_capture",
    "pending_recovery",
    "current_evidence_incomplete",
    "shared_file_links",
    "selection_source_mismatch",
    "selected_master_mismatch",
    "child_deleted",
    "child_lookup_failed",
    "child_identity_unresolved",
    "rendition_missing",
    "checksum_and_size_mismatch",
    "checksum_mismatch",
    "size_mismatch",
    "resource_unresolved",
    "success",
    "partial_failure",
    "session_expired",
    "interrupted",
    "starting",
    "running",
    "complete",
    "incomplete",
    "unknown",
    "not_recorded",
    "unavailable",
    "download",
    "first",
    "retry",
    "cleanup",
    "publication",
    "receipt_recovery",
    "failed",
    "pending",
    "downloaded",
    "expired_url",
    "authentication",
    "rate_limited",
    "request_failed",
    "malformed_response",
    "io",
    "state",
    "permission_denied",
    "not_found",
    "already_exists",
    "unsupported",
    "out_of_memory",
    "storage_full",
    "other",
    "ready",
    "startup",
    "shutdown",
    "configuration",
    "scan",
    "adoption",
    "ambiguous_collision",
    "strict_refusal",
    "hash_failure",
    "size_refusal",
    "no_match",
    "state_refusal",
    "native_exif",
    "xmp",
    "heif",
    "mtime",
    "capture",
    "embed",
    "sidecar",
    "proven_eof",
    "user_bound",
    "fetcher_error",
    "consumer_dropped",
    "malformed_record",
    "unpaired_records",
    "collecting",
    "streaming",
    "full",
    "incremental",
    "selected",
    "wider_inventory",
    "original",
    "medium",
    "thumb",
    "adjusted",
    "none",
    "as-is",
    "prefer-original",
    "prefer-alternative",
    "name-size-dedup",
    "name-size",
    "checksum",
    "all",
    "still",
    "mov",
    "both",
    "suffix",
    "primary",
    "per-pass",
    "global",
    "asset",
    "date",
    "preserve",
    "no_stored_token",
    "metadata_backfill",
    "enum_config_hash_drift",
    "download_config_hash_drift",
    "explicit_retry_failed",
    "other_static_reason",
    "kei",
    "icloud",
    "continue_tail",
    "retry_passes",
    "revalidate_records",
    "replay_from_prior_token",
    "await_retained_checkpoint_evidence",
    "retained_checkpoint_retry_exhausted",
    "repair_retained_checkpoint_evidence",
    "reconcile_inventory",
    "reauthenticate",
    "stop",
    "provider_checkpoint_preserved",
    "prior",
    "successor",
    "bridge",
    "fallback",
    "preserved",
    "advanced",
    "veto",
    "provider",
    "bounded",
    "unbounded",
    "rejected",
];

fn valid_alias(value: &str) -> bool {
    if let Some(n) = value.strip_prefix("alias-") {
        return n.parse::<u32>().is_ok_and(|n| n > 0 && n <= 4096);
    }
    value.split_once('/').is_some_and(|(namespace, ordinal)| {
        uuid::Uuid::parse_str(namespace).is_ok()
            && ordinal.parse::<usize>().is_ok_and(|n| n > 0 && n <= 128)
    })
}
