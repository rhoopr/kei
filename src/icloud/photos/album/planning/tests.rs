use super::{
    FetcherRange, FetcherRangeRole, PhotoStreamProfile, build_enumeration_plan,
    determine_fetcher_count, download_stream_page_size,
};

#[test]
fn test_fetcher_count_single_page() {
    // 50 items, page_size 100, concurrency 10 → 1 page → 1 fetcher
    assert_eq!(determine_fetcher_count(50, 100, 10), 1);
}

#[test]
fn test_fetcher_count_exact_pages() {
    // 500 items, page_size 100, concurrency 10 → 5 pages → 5 fetchers
    assert_eq!(determine_fetcher_count(500, 100, 10), 5);
}

#[test]
fn test_fetcher_count_capped_by_concurrency() {
    // 5000 items, page_size 100, concurrency 10 → 50 pages → capped to 10
    assert_eq!(determine_fetcher_count(5000, 100, 10), 10);
}

#[test]
fn test_fetcher_count_more_pages_than_concurrency() {
    // 50000 items, page_size 100, concurrency 10 → 500 pages → capped to 10
    assert_eq!(determine_fetcher_count(50000, 100, 10), 10);
}

#[test]
fn test_fetcher_count_zero_items() {
    // 0 items → at least 1 fetcher (the loop will just exit immediately)
    assert_eq!(determine_fetcher_count(0, 100, 10), 1);
}

#[test]
fn test_fetcher_count_concurrency_one() {
    // concurrency=1 always gives 1 fetcher
    assert_eq!(determine_fetcher_count(50000, 100, 1), 1);
}

#[test]
fn download_stream_page_size_stays_near_worker_pool() {
    assert_eq!(download_stream_page_size(100, 1), 2);
    assert_eq!(download_stream_page_size(100, 4), 8);
    assert_eq!(download_stream_page_size(100, 50), 100);
    assert_eq!(download_stream_page_size(0, 4), 1);
}

#[test]
fn download_profile_plan_covers_entire_recent_window_with_reduced_pages() {
    let plan = build_enumeration_plan(
        Some(1000),
        Some(5000),
        100,
        PhotoStreamProfile::BackpressuredDownload {
            download_concurrency: 10,
        },
    );

    assert_eq!(plan.page_size, 20);
    assert_eq!(
        plan.ranges,
        vec![FetcherRange {
            start: 0,
            end: u64::MAX,
            limit: Some(1000),
            role: FetcherRangeRole::LimitProbe,
        }]
    );
    assert!(
        plan.covers_prefix(1000),
        "download profile must keep complete recent-window coverage"
    );
}

#[test]
fn fast_profile_uses_ordered_limit_probe_for_recent_window() {
    let plan = build_enumeration_plan(
        Some(1000),
        Some(5000),
        100,
        PhotoStreamProfile::FastEnumeration { concurrency: 10 },
    );

    assert_eq!(plan.page_size, 100);
    assert_eq!(plan.ranges.len(), 1);
    assert_eq!(plan.ranges[0].role, FetcherRangeRole::LimitProbe);
    assert!(
        plan.covers_prefix(1000),
        "parallel fast profile must cover the same recent window"
    );
}

#[test]
fn limit_probe_does_not_trust_equal_count_as_eof() {
    let plan = build_enumeration_plan(
        Some(100),
        Some(100),
        100,
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
    );

    assert_eq!(
        plan.ranges,
        vec![FetcherRange {
            start: 0,
            end: u64::MAX,
            limit: Some(100),
            role: FetcherRangeRole::LimitProbe,
        }]
    );
}

#[test]
fn parallel_recent_plan_still_uses_one_ordered_limit_probe() {
    let plan = build_enumeration_plan(
        Some(100),
        Some(100),
        100,
        PhotoStreamProfile::FastEnumeration { concurrency: 10 },
    );

    assert_eq!(plan.ranges.len(), 1);
    assert_eq!(plan.ranges[0].start, 0);
    assert_eq!(plan.ranges[0].end, u64::MAX);
    assert_eq!(plan.ranges[0].role, FetcherRangeRole::LimitProbe);
    assert!(plan.covers_prefix(100));
}

#[test]
fn unbounded_parallel_plan_partitions_data_and_appends_tail_owner() {
    let plan = build_enumeration_plan(
        None,
        Some(5000),
        100,
        PhotoStreamProfile::FastEnumeration { concurrency: 10 },
    );

    assert!(
        plan.ranges
            .iter()
            .any(|range| range.start == 0 && range.role == FetcherRangeRole::Data),
        "the count prefix must keep count-partitioned data work"
    );
    assert_eq!(
        plan.ranges.last(),
        Some(&FetcherRange {
            start: 5000,
            end: u64::MAX,
            limit: None,
            role: FetcherRangeRole::TailProof,
        })
    );
    assert!(plan.covers_prefix(5000));
}

#[test]
fn test_fetcher_count_partial_page() {
    // 150 items, page_size 100 → 2 pages, concurrency 10 → 2 fetchers
    assert_eq!(determine_fetcher_count(150, 100, 10), 2);
}
