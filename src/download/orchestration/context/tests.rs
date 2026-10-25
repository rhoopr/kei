use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::icloud::photos::PhotoAsset;

use super::super::test_support::{
    mock_asset_record_for, mock_master_record_with_filename, mock_photo_records_with_filename,
};
use super::retain_legacy_added_date;
use super::{DownloadContext, count_value_map_entries, count_version_set_entries};

/// A tombstoned row cannot be refreshed, so no staleness check may
/// report it as needing one.
#[test]
fn staleness_checks_ignore_soft_deleted_assets() {
    let metadata = crate::state::MetadataCapture {
        shared: Arc::new(crate::state::AssetMetadata::default()),
        renditions: Arc::from([]),
    };
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset1".into())
        .or_default()
        .insert("original".into());
    ctx.metadata_retry_markers
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset1".into())
        .or_default()
        .insert("original".into());

    assert!(ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata));
    assert!(ctx.has_downloaded_metadata_retry_marker("PrimarySync", "asset1"));

    ctx.soft_deleted_ids
        .entry("PrimarySync".into())
        .or_default()
        .insert("asset1".into());
    assert!(!ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata));
    assert!(!ctx.has_downloaded_metadata_retry_marker("PrimarySync", "asset1"));
}

#[test]
fn legacy_master_state_requires_compatible_unambiguous_history() {
    let master = mock_master_record_with_filename("master-family", "family.jpg");
    let asset_a = PhotoAsset::new(
        master.clone(),
        mock_asset_record_for("asset-a", "master-family"),
    );
    let asset_b = PhotoAsset::new(master, mock_asset_record_for("asset-b", "master-family"));
    let checksum = asset_a
        .versions()
        .first()
        .expect("mock asset has an original version")
        .1
        .checksum
        .clone();

    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("master-family".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("master-family".into())
        .or_default()
        .insert("original".into(), checksum);

    retain_legacy_added_date(
        &mut ctx.legacy_added_dates,
        &Arc::from("PrimarySync"),
        &Arc::from("master-family"),
        asset_a.added_date_evidence(),
    );

    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));

    ctx.asset_master_mappings = Some(FxHashMap::default());
    ctx.legacy_master_state_owners = Some(FxHashMap::default());
    assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
    let dates = ctx.legacy_added_dates.get_mut("PrimarySync").unwrap();
    dates.insert("master-family".into(), None);
    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
    ctx.legacy_added_dates
        .get_mut("PrimarySync")
        .unwrap()
        .insert("master-family".into(), Some(chrono::DateTime::UNIX_EPOCH));
    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
    ctx.legacy_added_dates
        .get_mut("PrimarySync")
        .unwrap()
        .insert("master-family".into(), asset_a.added_date_evidence());

    ctx.asset_master_mappings
        .as_mut()
        .expect("mapping snapshot")
        .entry("PrimarySync".into())
        .or_default()
        .entry("master-family".into())
        .or_default()
        .extend([Arc::from("asset-a"), Arc::from("asset-b")]);
    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_b));

    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset-b".into())
        .or_default()
        .insert("original".into());
    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_b));

    ctx.legacy_master_state_owners
        .as_mut()
        .expect("owner snapshot")
        .entry("PrimarySync".into())
        .or_default()
        .insert("master-family".into(), "asset-a".into());
    assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_b));
}

#[test]
fn legacy_master_state_accepts_matching_pending_retry() {
    let records = mock_photo_records_with_filename("master-pending", "pending.jpg");
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let checksum = asset
        .versions()
        .first()
        .expect("mock asset has an original version")
        .1
        .checksum
        .clone();
    let mut ctx = DownloadContext {
        asset_master_mappings: Some(FxHashMap::default()),
        legacy_master_state_owners: Some(FxHashMap::default()),
        ..DownloadContext::default()
    };
    ctx.pending_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("master-pending".into())
        .or_default()
        .insert("original".into());
    ctx.pending_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("master-pending".into())
        .or_default()
        .insert("original".into(), checksum);

    retain_legacy_added_date(
        &mut ctx.legacy_added_dates,
        &Arc::from("PrimarySync"),
        &Arc::from("master-pending"),
        asset.added_date_evidence(),
    );

    assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset));
}

#[test]
fn test_download_context_known_ids_populated_for_retry_only() {
    // Simulate retry-only mode: known_ids is populated
    let mut ctx = DownloadContext::default();
    ctx.known_ids
        .entry("PrimarySync".into())
        .or_default()
        .insert("known_asset".into());

    // A known asset that's not in downloaded_ids needs download

    // The known_ids set is used externally to decide whether to skip new assets;
    // verify the set membership works
    assert!(ctx.is_known("PrimarySync", "known_asset"));
    assert!(!ctx.is_known("PrimarySync", "new_asset"));
    assert!(
        !ctx.is_known("SharedSync-AAAA", "known_asset"),
        "retry-only known IDs are library-scoped"
    );
}

#[test]
fn download_context_detects_downloaded_rows_missing_metadata_hashes() {
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_meta".into())
        .or_default()
        .insert("original".into());

    ctx.downloaded_without_metadata_hash = count_version_set_entries(&ctx.downloaded_ids)
        > count_value_map_entries(&ctx.downloaded_metadata_hashes);

    assert!(
        ctx.has_downloaded_without_metadata_hash(),
        "a downloaded row with no matching metadata hash needs the backfill notice"
    );

    ctx.downloaded_metadata_hashes
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_meta".into())
        .or_default()
        .insert("original".into(), "metadata_hash".into());

    ctx.downloaded_without_metadata_hash = count_version_set_entries(&ctx.downloaded_ids)
        > count_value_map_entries(&ctx.downloaded_metadata_hashes);

    assert!(
        !ctx.has_downloaded_without_metadata_hash(),
        "matching downloaded and metadata-hash sets should avoid the extra SQLite scan"
    );
}

// ── Gap coverage: empty versions, path traversal, empty filename ───

// ── Gap: DownloadContext attempt_counts used by producer ──────────

#[test]
fn download_context_attempt_counts_track_per_asset() {
    let mut ctx = DownloadContext::default();
    ctx.attempt_counts
        .entry("PrimarySync".into())
        .or_default()
        .insert("asset_high".into(), 15);
    ctx.attempt_counts
        .entry("PrimarySync".into())
        .or_default()
        .insert("asset_low".into(), 2);
    ctx.attempt_counts
        .entry("SharedSync-AAAA".into())
        .or_default()
        .insert("asset_high".into(), 1);

    // Simulate the producer's retry-exhaustion check
    let max_attempts = 10u32;
    assert!(
        ctx.attempt_count("PrimarySync", "asset_high")
            .is_some_and(|c| c >= max_attempts),
        "asset_high should exceed max_download_attempts"
    );
    assert!(
        ctx.attempt_count("PrimarySync", "asset_low")
            .is_none_or(|c| c < max_attempts),
        "asset_low should not exceed max_download_attempts"
    );
    assert!(
        ctx.attempt_count("SharedSync-AAAA", "asset_high")
            .is_none_or(|c| c < max_attempts),
        "same asset id in another library should not inherit primary attempts"
    );
    assert!(
        ctx.attempt_count("PrimarySync", "asset_never_failed")
            .is_none(),
        "unknown asset should not be in attempt_counts"
    );
}

// ── Mutation-pinning sibling: operator inversion ──────────
//
// The existing tests already assert the decision (Some(true) /
// Some(false)), not just field equality. What's missing is a test
// that pins the checksum-comparison **operator**: if a refactor
// swaps `stored.as_ref() != checksum` for `==`, the decision
// inverts and every downloaded asset re-downloads (or vice versa).
//
// stored-vs-current checksum line. With the existing fixtures,
// both sides happen to land on `Some(false)` for the matching
// case via the trust_state path, so the assertion below is the
// only one in the suite that fails on operator inversion: paired
// probes with opposite checksum equality, asserting opposite
// decisions.

// ── Gap: retry_only mode filters new assets ──────────────────────

#[test]
fn download_context_retry_only_known_ids_filtering() {
    let mut ctx = DownloadContext::default();
    ctx.known_ids
        .entry("PrimarySync".into())
        .or_default()
        .insert("previously_synced".into());

    // The producer checks known_ids separately before forwarding:
    assert!(ctx.is_known("PrimarySync", "previously_synced"));
    assert!(
        !ctx.is_known("PrimarySync", "brand_new_asset"),
        "new asset should not be in known_ids in retry_only mode"
    );
    assert!(
        !ctx.is_known("SharedSync-AAAA", "previously_synced"),
        "same asset id in another library should stay retry-only-new"
    );
}
