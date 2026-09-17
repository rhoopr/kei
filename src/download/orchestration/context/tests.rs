use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::icloud::photos::PhotoAsset;
use crate::state::VersionSizeKey;

use super::super::test_support::{
    mock_asset_record_for, mock_master_record_with_filename, mock_photo_records_with_filename,
};
use super::{DownloadContext, count_value_map_entries, count_version_set_entries};

#[test]
fn test_should_download_fast_trust_state_returns_false() {
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset1".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset1".into())
        .or_default()
        .insert("original".into(), "checksum_a".into());

    // trust_state=true: returns Some(false) for matching asset
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset1",
            VersionSizeKey::Original,
            "checksum_a",
            true
        ),
        Some(false)
    );

    // trust_state=false: returns None (needs filesystem check)
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset1",
            VersionSizeKey::Original,
            "checksum_a",
            false
        ),
        None
    );

    // Changed checksum: returns Some(true) regardless of trust_state
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset1",
            VersionSizeKey::Original,
            "checksum_b",
            true
        ),
        Some(true)
    );

    // Unknown asset: returns Some(true)
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "unknown",
            VersionSizeKey::Original,
            "x",
            true
        ),
        Some(true)
    );
}

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
    assert!(ctx.needs_metadata_rewrite(
        "PrimarySync",
        "asset1",
        VersionSizeKey::Original,
        Some("metadata-b")
    ));

    ctx.soft_deleted_ids
        .entry("PrimarySync".into())
        .or_default()
        .insert("asset1".into());

    assert!(!ctx.has_provider_metadata_drift("PrimarySync", "asset1", &metadata));
    assert!(!ctx.has_downloaded_metadata_retry_marker("PrimarySync", "asset1"));
    assert!(!ctx.needs_metadata_rewrite(
        "PrimarySync",
        "asset1",
        VersionSizeKey::Original,
        Some("metadata-b")
    ));
}

// ── should_download_fast additional tests ───────────────────────────

#[test]
fn test_should_download_fast_unknown_asset_returns_true() {
    let ctx = DownloadContext::default();
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "never_seen",
            VersionSizeKey::Original,
            "any_ck",
            true
        ),
        Some(true)
    );
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "never_seen",
            VersionSizeKey::Original,
            "any_ck",
            false
        ),
        Some(true)
    );
}

#[test]
fn needs_metadata_rewrite_detects_hash_change() {
    let mut ctx = DownloadContext::default();
    ctx.downloaded_metadata_hashes
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_md".into())
        .or_default()
        .insert("original".into(), "hash-OLD".into());

    // Same hash -> no rewrite needed.
    assert!(!ctx.needs_metadata_rewrite(
        "PrimarySync",
        "asset_md",
        VersionSizeKey::Original,
        Some("hash-OLD")
    ));
    // Different hash -> rewrite.
    assert!(ctx.needs_metadata_rewrite(
        "PrimarySync",
        "asset_md",
        VersionSizeKey::Original,
        Some("hash-NEW")
    ));
    // Unknown new hash -> no rewrite (nothing to compare to).
    assert!(!ctx.needs_metadata_rewrite("PrimarySync", "asset_md", VersionSizeKey::Original, None));
}

#[test]
fn needs_metadata_rewrite_honors_retry_marker() {
    let mut ctx = DownloadContext::default();
    ctx.metadata_retry_markers
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_retry".into())
        .or_default()
        .insert("original".into());
    // No stored hash at all, but marker is set -> rewrite needed.
    assert!(ctx.needs_metadata_rewrite(
        "PrimarySync",
        "asset_retry",
        VersionSizeKey::Original,
        None
    ));
    // Marker set -> rewrite even if hashes match.
    ctx.downloaded_metadata_hashes
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_retry".into())
        .or_default()
        .insert("original".into(), "h".into());
    assert!(ctx.needs_metadata_rewrite(
        "PrimarySync",
        "asset_retry",
        VersionSizeKey::Original,
        Some("h")
    ));
}

#[test]
fn needs_metadata_rewrite_refreshes_null_stored_hash() {
    // Pre-v5 downloaded rows have metadata_hash IS NULL; even without a
    // retry marker, a fresh hash should trigger a rewrite so the XMP
    // gets the source state this tree has never recorded.
    let ctx = DownloadContext::default();
    assert!(ctx.needs_metadata_rewrite(
        "PrimarySync",
        "asset_no_stored_hash",
        VersionSizeKey::Original,
        Some("new-hash")
    ));
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

    assert!(!ctx.should_use_legacy_master_state("PrimarySync", &asset_a));

    ctx.asset_master_mappings = Some(FxHashMap::default());
    ctx.legacy_master_state_owners = Some(FxHashMap::default());
    assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset_a));

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
    assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset_a));
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

    assert!(ctx.should_use_legacy_master_state("PrimarySync", &asset));
}

#[test]
fn test_should_download_fast_downloaded_matching_checksum() {
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_x".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_x".into())
        .or_default()
        .insert("original".into(), "ck_match".into());

    // trust_state=true => hard skip
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_x",
            VersionSizeKey::Original,
            "ck_match",
            true
        ),
        Some(false)
    );
    // trust_state=false => needs filesystem check
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_x",
            VersionSizeKey::Original,
            "ck_match",
            false
        ),
        None
    );
}

#[test]
fn test_should_download_fast_downloaded_changed_checksum() {
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_y".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_y".into())
        .or_default()
        .insert("original".into(), "old_ck".into());

    // Changed checksum => needs re-download regardless of trust_state
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_y",
            VersionSizeKey::Original,
            "new_ck",
            true
        ),
        Some(true)
    );
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_y",
            VersionSizeKey::Original,
            "new_ck",
            false
        ),
        Some(true)
    );
}

#[test]
fn test_should_download_fast_different_version_size() {
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_z".into())
        .or_default()
        .insert("original".into());

    // Medium version not downloaded
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_z",
            VersionSizeKey::Medium,
            "any_ck",
            true
        ),
        Some(true)
    );
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
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "known_asset",
            VersionSizeKey::Original,
            "ck",
            true
        ),
        Some(true)
    );
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

#[test]
fn should_download_fast_empty_checksum_never_hard_skips() {
    // Empty remote checksum is malformed provider input. Even if a stale
    // DB row also has an empty checksum, this must not turn into a hard
    // skip: the provider parser rejects new empty checksums, and this
    // fast path stays defensive for legacy/corrupt state.
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_empty_ck".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_empty_ck".into())
        .or_default()
        .insert("original".into(), "".into());

    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_empty_ck",
            VersionSizeKey::Original,
            "",
            true
        ),
        Some(true)
    );
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_empty_ck",
            VersionSizeKey::Original,
            "",
            false
        ),
        Some(true)
    );
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_empty_ck",
            VersionSizeKey::Original,
            "abc123def456",
            true,
        ),
        Some(true)
    );
}

// ── Gap coverage: should_download_fast with no checksum in DB ────────

#[test]
fn should_download_fast_no_checksum_trust_true_returns_false() {
    // Asset is in downloaded_ids but has no entry in downloaded_checksums.
    // With trust_state=true the method should hard-skip (Some(false))
    // because the absence of a stored checksum means "nothing to compare".
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_no_ck".into())
        .or_default()
        .insert("original".into());
    // No entry in downloaded_checksums

    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_no_ck",
            VersionSizeKey::Original,
            "any",
            true
        ),
        Some(false)
    );
}

#[test]
fn should_download_fast_no_checksum_trust_false_returns_none() {
    // Same scenario but trust_state=false: needs filesystem check (None).
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_no_ck".into())
        .or_default()
        .insert("original".into());

    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_no_ck",
            VersionSizeKey::Original,
            "any",
            false
        ),
        None
    );
}

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

// ── Gap: should_download_fast with downloaded but different version ──

#[test]
fn should_download_fast_downloaded_original_but_medium_requested() {
    // Asset is downloaded as Original, but now we ask about Medium.
    // should_download_fast should return Some(true) because Medium
    // was never downloaded.
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_multi".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_multi".into())
        .or_default()
        .insert("original".into(), "ck_orig".into());

    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "asset_multi",
            VersionSizeKey::Medium,
            "ck_med",
            true
        ),
        Some(true),
        "Medium version not in downloaded set should need download"
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
// Mutation: in `should_download_fast`, swap `!=` to `==` on the
// stored-vs-current checksum line. With the existing fixtures,
// both sides happen to land on `Some(false)` for the matching
// case via the trust_state path, so the assertion below is the
// only one in the suite that fails on operator inversion: paired
// probes with opposite checksum equality, asserting opposite
// decisions.
#[test]
fn should_download_fast_inverts_when_checksum_operator_flips() {
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_op".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("asset_op".into())
        .or_default()
        .insert("original".into(), "ck_stored".into());

    // Matching checksum + trust_state=true → skip (Some(false)).
    let matching = ctx.should_download_fast(
        "PrimarySync",
        "asset_op",
        VersionSizeKey::Original,
        "ck_stored",
        true,
    );
    // Different checksum + trust_state=true → re-download
    // (Some(true)).
    let different = ctx.should_download_fast(
        "PrimarySync",
        "asset_op",
        VersionSizeKey::Original,
        "ck_changed",
        true,
    );

    // Pin the inversion. If `!=` were swapped for `==`, both probes
    // would return the same Some(_) — collapsing the decision
    // surface. Asserting opposite values catches that.
    assert_eq!(matching, Some(false), "matching checksum must skip");
    assert_eq!(different, Some(true), "changed checksum must re-download");
    assert_ne!(
        matching, different,
        "matching and changed checksums must produce opposite decisions \
         (catches `!=` ↔ `==` operator swap on stored-vs-current compare)"
    );
}

// ── Gap: should_download_fast with multiple version sizes ─────────

#[test]
fn should_download_fast_multiple_versions_independent() {
    // Both Original and LiveOriginal downloaded, each with own checksum.
    let mut ctx = DownloadContext::default();
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("live_asset".into())
        .or_default()
        .insert("original".into());
    ctx.downloaded_ids
        .entry("PrimarySync".into())
        .or_default()
        .entry("live_asset".into())
        .or_default()
        .insert("live_original".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("live_asset".into())
        .or_default()
        .insert("original".into(), "ck_img".into());
    ctx.downloaded_checksums
        .entry("PrimarySync".into())
        .or_default()
        .entry("live_asset".into())
        .or_default()
        .insert("live_original".into(), "ck_mov".into());

    // Image: matching checksum, trusted
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "live_asset",
            VersionSizeKey::Original,
            "ck_img",
            true
        ),
        Some(false)
    );
    // MOV: matching checksum, trusted
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "live_asset",
            VersionSizeKey::LiveOriginal,
            "ck_mov",
            true
        ),
        Some(false)
    );
    // MOV: changed checksum -- re-download even though image is fine
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "live_asset",
            VersionSizeKey::LiveOriginal,
            "ck_mov_v2",
            true
        ),
        Some(true),
        "changed MOV checksum should trigger re-download"
    );
}

// ── Gap: retry_only mode filters new assets ──────────────────────

#[test]
fn download_context_retry_only_known_ids_filtering() {
    let mut ctx = DownloadContext::default();
    ctx.known_ids
        .entry("PrimarySync".into())
        .or_default()
        .insert("previously_synced".into());

    // Known asset: should_download_fast returns Some(true) (it needs
    // download because it's not in downloaded_ids)
    assert_eq!(
        ctx.should_download_fast(
            "PrimarySync",
            "previously_synced",
            VersionSizeKey::Original,
            "ck",
            true
        ),
        Some(true)
    );
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
