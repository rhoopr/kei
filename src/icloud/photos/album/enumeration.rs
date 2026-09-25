//! Full-enumeration channels, fetcher tasks, and token delivery.

use super::completion::{FetcherSyncTokenCapture, await_fetcher_handles};
use super::fetch::FetcherBehavior;
use super::planning::{PhotoStreamProfile, build_enumeration_plan, effective_total};
use super::{PhotoAlbum, PhotoStream};
use crate::icloud::photos::asset::PhotoAsset;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

impl PhotoAlbum {
    pub(super) fn photo_stream_with_token_inner_no_cross_zone(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        profile: PhotoStreamProfile,
        preserve_blank_sync_tokens_for_diagnostics: bool,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let fetcher_sync_tokens = Arc::new(FetcherSyncTokenCapture::default());

        let (stream, handles) = self.photo_stream_inner(
            limit,
            total_count,
            profile,
            Some(fetcher_sync_tokens.clone()),
            preserve_blank_sync_tokens_for_diagnostics,
            treat_empty_tail_as_error,
        );
        let album_name = Arc::clone(&self.name);

        tokio::spawn(async move {
            let fetcher_panicked = await_fetcher_handles(handles).await;
            let final_token = if fetcher_panicked {
                None
            } else {
                fetcher_sync_tokens.resolve(&album_name).await
            };
            let _ = token_tx.send(final_token);
        });

        (stream, token_rx)
    }

    /// Shared implementation for `photo_stream` and `photo_stream_with_token`.
    ///
    /// When `fetcher_sync_tokens` is `Some`, each fetcher appends its last
    /// observed `syncToken` to the shared token list.
    ///
    /// Returns the stream and all spawned fetcher `JoinHandle`s.
    pub(super) fn photo_stream_inner(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        profile: PhotoStreamProfile,
        fetcher_sync_tokens: Option<Arc<FetcherSyncTokenCapture>>,
        preserve_blank_sync_tokens_for_diagnostics: bool,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, Vec<JoinHandle<()>>) {
        let plan = build_enumeration_plan(limit, total_count, self.page_size, profile);
        if let Some(capture) = &fetcher_sync_tokens {
            capture.expect(plan.ranges.len());
            if matches!((limit, total_count), (Some(limit), Some(total)) if total > u64::from(limit))
            {
                capture.suppress();
            }
        }
        let (tx, rx) = mpsc::channel::<anyhow::Result<PhotoAsset>>(
            (plan.page_size * plan.channel_fetchers()).min(500),
        );
        let range_record_owners =
            Arc::new(std::sync::Mutex::new(FxHashMap::<String, u64>::default()));
        let mut handles = Vec::with_capacity(plan.ranges.len());

        if effective_total(limit, total_count).is_none() {
            tracing::info!(target: "kei::icloud::photos::album", "Fetching photos from iCloud...");
        }

        let allow_unpaired_at_range_boundary = plan.ranges.len() > 1;
        let behavior = FetcherBehavior {
            page_size: plan.page_size,
            preserve_blank_sync_tokens_for_diagnostics,
            allow_unpaired_at_range_boundary,
            treat_empty_tail_as_error,
        };
        for range in plan.ranges {
            handles.push(self.spawn_fetcher(
                tx.clone(),
                range,
                Arc::clone(&range_record_owners),
                fetcher_sync_tokens.clone(),
                behavior,
            ));
        }
        // Drop our sender so channel closes when all fetchers finish.
        drop(tx);

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            handles,
        )
    }
}

#[cfg(test)]
mod tests;
