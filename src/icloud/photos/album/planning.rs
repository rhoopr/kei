//! Full-enumeration profiles and rank-range planning.

/// Keep signed CDN URLs close to the download workers that will consume them.
///
/// A normal CloudKit page is optimized for fast enumeration, but each
/// `PhotoAsset` also carries short-lived content URLs. During real downloads,
/// fetch only a small number of waves ahead of the worker pool so slow media
/// cannot age thousands of prefetched URLs before transfer starts.
fn download_stream_page_size(default_page_size: usize, download_concurrency: usize) -> usize {
    default_page_size
        .min(download_concurrency.max(1).saturating_mul(2))
        .max(1)
}

/// Determine how many parallel fetcher tasks to spawn.
///
/// We never spawn more fetchers than total pages (no empty fetchers)
/// and never more than the requested concurrency level.
fn determine_fetcher_count(total_items: u64, page_size: usize, concurrency: usize) -> usize {
    let total_pages = total_items.div_ceil(page_size as u64);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded to concurrency (usize) immediately via .min()"
    )]
    let pages_as_usize = total_pages as usize;
    pages_as_usize.min(concurrency).max(1)
}

/// Profile for CloudKit record enumeration.
///
/// This intentionally separates *which ranks are enumerated* from *how much
/// URL-bearing data is allowed to sit ahead of the downloader*. Download mode
/// can use smaller pages and less concurrency to keep signed CDN URLs fresh,
/// but it must not change the covered rank range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PhotoStreamProfile {
    FastEnumeration { concurrency: usize },
    BackpressuredDownload { download_concurrency: usize },
}

impl PhotoStreamProfile {
    fn request_page_size(self, default_page_size: usize) -> usize {
        match self {
            Self::FastEnumeration { .. } => default_page_size.max(1),
            Self::BackpressuredDownload {
                download_concurrency,
            } => download_stream_page_size(default_page_size, download_concurrency),
        }
    }

    fn fetcher_concurrency(self) -> usize {
        match self {
            Self::FastEnumeration { concurrency } => concurrency.max(1),
            Self::BackpressuredDownload { .. } => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FetcherRangeRole {
    /// Count-partitioned work that covers a known rank interval.
    Data,
    /// The single owner that scans past the count hint until natural EOF.
    TailProof,
    /// A bounded stream that must inspect one more eligible asset to prove
    /// whether the caller's limit truncated the inventory.
    LimitProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FetcherRange {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) limit: Option<u32>,
    pub(super) role: FetcherRangeRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EnumerationPlan {
    pub(super) page_size: usize,
    pub(super) ranges: Vec<FetcherRange>,
}

impl EnumerationPlan {
    pub(super) fn channel_fetchers(&self) -> usize {
        self.ranges.len().max(1)
    }

    #[cfg(test)]
    fn covers_prefix(&self, end: u64) -> bool {
        if end == 0 {
            return true;
        }

        let mut ranges = self.ranges.clone();
        ranges.sort_unstable_by_key(|range| range.start);
        let mut cursor = 0;
        for range in ranges {
            if range.end <= cursor {
                continue;
            }
            if range.start > cursor {
                return false;
            }
            cursor = range.end;
            if cursor >= end {
                return true;
            }
        }
        false
    }
}

pub(super) fn effective_total(limit: Option<u32>, total_count: Option<u64>) -> Option<u64> {
    total_count
        .map(|tc| limit.map_or(tc, |lim| tc.min(u64::from(lim))))
        .or_else(|| limit.map(u64::from))
}

fn range_limit(limit: Option<u32>, start: u64, end: u64) -> Option<u32> {
    limit.map(|lim| {
        let remaining = u64::from(lim).saturating_sub(start);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "bounded by min(end-start, limit) where both operands originated from u32 fetcher limits"
        )]
        {
            remaining.min(end.saturating_sub(start)) as u32
        }
    })
}

fn push_fetcher_range(ranges: &mut Vec<FetcherRange>, limit: Option<u32>, start: u64, end: u64) {
    if start >= end {
        return;
    }
    ranges.push(FetcherRange {
        start,
        end,
        limit: range_limit(limit, start, end),
        role: FetcherRangeRole::Data,
    });
}

pub(super) fn build_enumeration_plan(
    limit: Option<u32>,
    total_count: Option<u64>,
    default_page_size: usize,
    profile: PhotoStreamProfile,
) -> EnumerationPlan {
    let page_size = profile.request_page_size(default_page_size);
    // A caller limit needs an ordered N+1 probe. Running count-partitioned
    // ranges in parallel cannot prove which asset is the first item beyond
    // the bound, and historically `limit == count` was incorrectly treated
    // as EOF. Keep bounded streams sequential and use the count only as a
    // scheduling hint for unbounded inventories.
    if let Some(limit) = limit {
        return EnumerationPlan {
            page_size,
            ranges: vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: Some(limit),
                role: FetcherRangeRole::LimitProbe,
            }],
        };
    }

    let Some(total) = total_count else {
        return EnumerationPlan {
            page_size,
            ranges: vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: None,
                role: FetcherRangeRole::TailProof,
            }],
        };
    };

    // Download streams deliberately keep one ordered fetcher so signed URLs
    // stay near their consumers. Let that fetcher own EOF proof directly
    // rather than stopping at the count hint and starting a concurrent tail.
    if matches!(profile, PhotoStreamProfile::BackpressuredDownload { .. })
        || profile.fetcher_concurrency() == 1
    {
        return EnumerationPlan {
            page_size,
            ranges: vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: None,
                role: FetcherRangeRole::TailProof,
            }],
        };
    }

    let concurrency = profile.fetcher_concurrency();
    let num_fetchers = if concurrency > 1 && total > 0 {
        determine_fetcher_count(total, page_size, concurrency * 2)
    } else {
        1
    };
    let chunk_size_items = {
        let raw = total.div_ceil(num_fetchers as u64);
        let ps = page_size as u64;
        raw.div_ceil(ps) * ps
    };

    let mut ranges = Vec::with_capacity(num_fetchers + 1);
    for i in 0..num_fetchers {
        let start = i as u64 * chunk_size_items;
        let end = ((i as u64 + 1) * chunk_size_items).min(total);
        if start >= total {
            break;
        }
        push_fetcher_range(&mut ranges, None, start, end);
    }

    // Counts are hints, not EOF proof. The final owner starts exactly where
    // the count-partitioned ranges stop and continues until the provider's
    // consecutive-empty-page policy proves natural EOF.
    ranges.push(FetcherRange {
        start: total,
        end: u64::MAX,
        limit: None,
        role: FetcherRangeRole::TailProof,
    });

    EnumerationPlan { page_size, ranges }
}

#[cfg(test)]
mod tests;
