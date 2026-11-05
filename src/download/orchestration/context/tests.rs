use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::icloud::photos::PhotoAsset;

use super::super::test_support::{
    mock_asset_record_for, mock_master_record_with_filename, mock_photo_records_with_filename,
};
use super::retain_legacy_added_date;
use super::{DownloadContext, count_value_map_entries, count_version_set_entries};

#[tokio::test]
async fn retained_rendition_refresh_keeps_measurements_and_quiet_drift() {
    use crate::state::{
        AssetMetadata, DownloadContextStateStore, DownloadStateStore, MetadataCapture,
        RenditionMetadata, SqliteStateDb, VersionSizeKey,
    };
    use crate::test_helpers::TestAssetRecord;
    use sha2::{Digest, Sha256};
    let key = VersionSizeKey::Adjusted;
    for case in [
        "absent",
        "none",
        "known",
        "conflict",
        "new-generation",
        "blank",
        "invalid",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let file = dir.path().join("retained.jpg");
        std::fs::write(&file, b"retained media").unwrap();
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(b"retained media"));
        let checksum = if case == "blank" { " \t" } else { "a" };
        let old = AssetMetadata {
            width: Some(640),
            height: Some(480),
            duration_secs: Some(1.25),
            title: Some("old".into()),
            ..Default::default()
        };
        let db = SqliteStateDb::open(&path).await.unwrap();
        let record = TestAssetRecord::new("retained")
            .version_size(key)
            .checksum(checksum)
            .size(b"retained media".len() as u64)
            .metadata(old)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "retained",
            key.as_str(),
            &file,
            &hash,
            Some(&hash),
            false,
        )
        .await
        .unwrap();
        let facts = |checksum: &str, width| RenditionMetadata {
            checksum: Some(Arc::from(checksum)),
            width,
            height: None,
            duration_secs: None,
        };
        let renditions = match case {
            "none" => vec![(key, facts("a", None))],
            "known" => vec![(key, facts("a", Some(800)))],
            "conflict" => vec![(key, facts("a", None)), (key, facts("a", Some(800)))],
            "new-generation" => vec![(key, facts("b", None))],
            _ => Vec::new(),
        };
        let capture = MetadataCapture {
            shared: Arc::new(AssetMetadata {
                title: Some("new".into()),
                ..Default::default()
            }),
            renditions: renditions.into(),
        };
        if case == "invalid" {
            db.acquire_lock("invalid measurement")
                .unwrap()
                .execute_batch("UPDATE assets SET width=-1")
                .unwrap();
        }
        let result = db
            .refresh_downloaded_asset_metadata(
                "PrimarySync",
                "retained",
                (&capture, record.created_at, record.added_at),
                false,
                false,
                1,
            )
            .await;
        if case == "invalid" {
            assert!(result.is_err());
            assert_eq!(
                db.get_downloaded_page(0, 10).await.unwrap()[0]
                    .metadata
                    .title
                    .as_deref(),
                Some("old")
            );
            eprintln!("retention case passed: {case}");
            continue;
        }
        assert_eq!(result.unwrap(), 1);
        drop(db);
        let db = SqliteStateDb::open(&path).await.unwrap();
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        let metadata = &rows[0].metadata;
        let expected = match case {
            "absent" | "new-generation" => (Some(640), Some(480), Some(1.25)),
            "known" => (Some(800), None, None),
            _ => (None, None, None),
        };
        assert_eq!(
            (metadata.width, metadata.height, metadata.duration_secs),
            expected,
            "{case}"
        );
        assert_eq!(
            metadata.duration_secs.map(f64::to_bits),
            expected.2.map(f64::to_bits)
        );
        assert_eq!(metadata.title.as_deref(), Some("new"));
        assert_eq!(metadata.metadata_hash, Some(metadata.compute_hash()));
        let proof = db.get_baseline_receipts().await.unwrap();
        assert_eq!(proof.len(), 1);
        assert_eq!(proof[0].provider_checksum, checksum);
        assert_eq!(proof[0].local_path, file);
        assert_eq!(proof[0].local_checksum.as_deref(), Some(hash.as_str()));
        assert_eq!(proof[0].source_checksum.as_deref(), Some(hash.as_str()));
        assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
        assert_eq!(std::fs::read(&file).unwrap(), b"retained media");
        let mut ctx = DownloadContext::load(&db, false).await;
        assert_eq!(
            ctx.has_provider_metadata_drift("PrimarySync", "retained", &capture),
            case == "blank"
        );
        let mut caption = capture.clone();
        Arc::make_mut(&mut caption.shared).title = Some("later".into());
        assert!(ctx.has_provider_metadata_drift("PrimarySync", "retained", &caption));
        if case == "absent" {
            let saved = ctx.downloaded_renditions.remove("PrimarySync").unwrap();
            assert!(ctx.has_provider_metadata_drift("PrimarySync", "retained", &capture));
            ctx.downloaded_renditions
                .insert("PrimarySync".into(), saved);
        }
        let versions = ctx.downloaded_checksums.get_mut("PrimarySync").unwrap();
        *versions
            .get_mut("retained")
            .unwrap()
            .get_mut("adjusted")
            .unwrap() = "mismatch".into();
        assert!(ctx.has_provider_metadata_drift("PrimarySync", "retained", &capture));
        if case == "new-generation" {
            let next = TestAssetRecord::new("retained")
                .version_size(key)
                .checksum("b")
                .metadata(capture.resolve(key, "b"))
                .build();
            db.upsert_seen(&next).await.unwrap();
            let pending = db.get_pending().await.unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].checksum.as_ref(), "b");
            assert_eq!(
                (
                    pending[0].metadata.width,
                    pending[0].metadata.height,
                    pending[0].metadata.duration_secs
                ),
                (None, None, None)
            );
        }
        eprintln!("retention case passed: {case}");
    }
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
