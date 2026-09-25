//! Named-album and durable-identity hydration across source zones.

use super::planning::PhotoStreamProfile;
use super::{PhotoAlbum, PhotoStream};
use crate::icloud::photos::asset::{ChangeEvent, DeltaRecordBuffer, PhotoAsset};
use rustc_hash::FxHashSet;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

impl PhotoAlbum {
    pub(super) fn photo_stream_with_cross_zone_hydration(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        profile: PhotoStreamProfile,
        preserve_blank_sync_tokens_for_diagnostics: bool,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        let (tx, rx) = mpsc::channel::<anyhow::Result<PhotoAsset>>(500);
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let (base_stream, base_token_rx) = self.photo_stream_with_token_inner_no_cross_zone(
            limit,
            total_count,
            profile,
            preserve_blank_sync_tokens_for_diagnostics,
            treat_empty_tail_as_error,
        );
        let Some(container_id) = self.container_id.clone() else {
            let _ = token_tx.send(None);
            return (
                Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
                token_rx,
            );
        };
        let album_name = Arc::clone(&self.name);
        let sources = self.cross_zone_sources_for_task();
        let owner = self.clone_for_task_without_sources();

        tokio::spawn(async move {
            use futures_util::StreamExt;

            let mut base_stream = Box::pin(base_stream);
            let mut seen_asset_records = FxHashSet::<String>::default();
            let mut base_seen = 0u64;
            let mut stream_error = false;

            while let Some(item) = base_stream.next().await {
                match item {
                    Ok(asset) => {
                        seen_asset_records.insert(asset.asset_record_name().to_string());
                        base_seen += 1;
                        if tx.send(Ok(asset)).await.is_err() {
                            let _ = token_tx.send(None);
                            return;
                        }
                    }
                    Err(e) => {
                        stream_error = true;
                        let _ = tx.send(Err(e)).await;
                    }
                }
            }

            let base_token = base_token_rx.await.ok().flatten();
            let should_hydrate =
                limit.is_none() && total_count.is_some_and(|expected| base_seen < expected);
            if stream_error {
                let _ = token_tx.send(None);
                return;
            }
            if !should_hydrate {
                let _ = token_tx.send(base_token);
                return;
            }

            let relation_ids = match owner.album_relation_item_ids(&container_id).await {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    let _ = token_tx.send(None);
                    return;
                }
            };
            let mut missing: FxHashSet<String> = relation_ids
                .into_iter()
                .filter(|id| !seen_asset_records.contains(id))
                .collect();
            if missing.is_empty() {
                let _ = token_tx.send(base_token);
                return;
            }

            tracing::info!(target: "kei::icloud::photos::album",
                album = %album_name,
                base_seen,
                missing = missing.len(),
                "Album relation records exceed owner-zone assets; checking bounded cross-zone sources"
            );

            let mut hydrated = 0usize;
            for source in sources {
                if missing.is_empty() {
                    break;
                }
                let missing_before = missing.len();
                match source.matching_assets_from_changes(&mut missing).await {
                    Ok(assets) => {
                        hydrated += missing_before.saturating_sub(missing.len());
                        for asset in assets {
                            if tx.send(Ok(asset)).await.is_err() {
                                let _ = token_tx.send(None);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        let _ = token_tx.send(None);
                        return;
                    }
                }
            }

            if hydrated > 0 {
                tracing::info!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    base_seen,
                    hydrated,
                    unresolved = missing.len(),
                    "Album spans multiple CloudKit zones; hydrated bounded cross-zone members"
                );
            }

            if !missing.is_empty() {
                let sample: Vec<&str> = missing.iter().take(5).map(String::as_str).collect();
                tracing::warn!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    unresolved = missing.len(),
                    sample = ?sample,
                    "Album has unresolved relation records; continuing with visible downloadable assets"
                );
            }

            let _ = token_tx.send(if missing.is_empty() { base_token } else { None });
        });

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            token_rx,
        )
    }

    fn cross_zone_sources_for_task(&self) -> Vec<PhotoAlbum> {
        self.cross_zone_sources
            .iter()
            .map(Self::clone_for_task_without_sources)
            .collect()
    }

    async fn album_relation_item_ids(
        &self,
        container_id: &str,
    ) -> anyhow::Result<FxHashSet<String>> {
        let mut ids = FxHashSet::default();
        self.scan_changes_zone(|record| {
            if record.record_type != "CPLContainerRelation" {
                return true;
            }
            if record.deleted == Some(true) {
                return true;
            }
            let container = record
                .fields
                .get("containerId")
                .and_then(|f| f.get("value"))
                .and_then(Value::as_str);
            if container != Some(container_id) {
                return true;
            }
            if let Some(item_id) = record
                .fields
                .get("itemId")
                .and_then(|f| f.get("value"))
                .and_then(Value::as_str)
            {
                ids.insert(item_id.to_string());
            }
            true
        })
        .await?;
        Ok(ids)
    }

    pub(super) async fn hydrate_assets(
        &self,
        missing_asset_record_names: &mut FxHashSet<String>,
    ) -> anyhow::Result<Vec<PhotoAsset>> {
        let mut matched = self
            .clone_for_task_without_sources()
            .matching_assets_from_changes(missing_asset_record_names)
            .await?;
        for source in self.cross_zone_sources_for_task() {
            if missing_asset_record_names.is_empty() {
                break;
            }
            matched.extend(
                source
                    .matching_assets_from_changes(missing_asset_record_names)
                    .await?,
            );
        }
        Ok(matched)
    }

    /// Recover current `CPLAsset` records for legacy state keyed only by a
    /// `CPLMaster` record name.
    ///
    /// Scans to EOF so the caller sees every current sibling before choosing
    /// one from durable version, size, and checksum evidence.
    pub(super) async fn hydrate_masters(
        &self,
        master_record_names: &FxHashSet<String>,
        shutdown_token: &CancellationToken,
    ) -> anyhow::Result<Vec<PhotoAsset>> {
        if master_record_names.is_empty() || shutdown_token.is_cancelled() {
            return Ok(Vec::new());
        }

        fn collect_event(
            event: ChangeEvent,
            source_zone: &Arc<str>,
            master_record_names: &FxHashSet<String>,
            seen_asset_record_names: &mut FxHashSet<String>,
            matched: &mut Vec<PhotoAsset>,
        ) {
            if event.reason != crate::types::ChangeReason::Created {
                return;
            }
            let Some(asset) = event.asset else {
                return;
            };
            let asset = asset.with_source_zone(Arc::clone(source_zone));
            if master_record_names.contains(asset.id())
                && seen_asset_record_names.insert(asset.asset_record_name().to_string())
            {
                matched.push(asset);
            }
        }

        let mut buffer = DeltaRecordBuffer::new();
        let source_zone: Arc<str> = Arc::from(self.zone_name());
        let mut seen_asset_record_names = FxHashSet::default();
        let mut matched = Vec::new();
        self.scan_changes_zone(|record| {
            for event in buffer.process_records(vec![record]) {
                collect_event(
                    event,
                    &source_zone,
                    master_record_names,
                    &mut seen_asset_record_names,
                    &mut matched,
                );
            }
            !shutdown_token.is_cancelled()
        })
        .await?;

        for event in buffer.flush() {
            collect_event(
                event,
                &source_zone,
                master_record_names,
                &mut seen_asset_record_names,
                &mut matched,
            );
        }
        Ok(matched)
    }

    async fn matching_assets_from_changes(
        &self,
        missing_asset_record_names: &mut FxHashSet<String>,
    ) -> anyhow::Result<Vec<PhotoAsset>> {
        let source_zone: Arc<str> = Arc::from(self.zone_name());
        let mut buffer = DeltaRecordBuffer::new();
        let mut matched = Vec::new();
        self.scan_changes_zone(|record| {
            let events = buffer.process_records(vec![record]);
            for event in events {
                let Some(asset) = event.asset else {
                    continue;
                };
                if missing_asset_record_names.remove(asset.asset_record_name()) {
                    let asset = asset.with_source_zone(Arc::clone(&source_zone));
                    matched.push(asset);
                }
            }
            !missing_asset_record_names.is_empty()
        })
        .await?;

        for event in buffer.flush() {
            let Some(asset) = event.asset else {
                continue;
            };
            if missing_asset_record_names.remove(asset.asset_record_name()) {
                let asset = asset.with_source_zone(Arc::clone(&source_zone));
                matched.push(asset);
            }
        }
        Ok(matched)
    }
}

#[cfg(test)]
mod tests;
