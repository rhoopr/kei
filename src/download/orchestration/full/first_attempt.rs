//! Schedule the initial full query. A planned pass has no token or EOF evidence.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_util::{StreamExt, stream};
use reqwest::Client;
use rustc_hash::FxHashSet;
use tokio_util::sync::CancellationToken;

use super::token_diagnostics::CompletionOrder;
use super::{
    AlbumSnapshotRecorder, DownloadConfig, DownloadContext, DownloadControls, DownloadPhotoStream,
    FullPassStreamOptions, PassCountPlan, PassKey, PassTokenReceiver, PerPassStreamingResult,
    StreamRuntime, StreamingResult, deferred_unfiled_index, filter_stream_to_enumeration_bounds,
    merge_streaming_result, open_photo_stream_for_controls, preload_download_context,
    progress_library_label, run_full_pass_stream, should_record_album_snapshots,
    stream_full_inventory, track_unfiled_heartbeat,
};
use crate::commands::{AlbumPass, PassKind};
use crate::personality::album_divider::AlbumDivider;

struct PlannedPass<'a> {
    index: usize,
    pass: &'a AlbumPass,
    config: Arc<DownloadConfig>,
    count: u64,
    stream_count: Option<u64>,
    label: &'a str,
    record_snapshot: bool,
}

enum Schedule {
    Merged(Arc<DownloadConfig>),
    PerPass {
        parallelism: usize,
        deferred: Option<usize>,
    },
}

pub(super) struct FirstAttempt<'a> {
    passes: Vec<PlannedPass<'a>>,
    schedule: Schedule,
    exact_total: Option<u64>,
}

pub(super) struct ObservedAttempt {
    pub(super) streaming: StreamingResult,
    pub(super) receivers: Vec<PassTokenReceiver>,
    pub(super) snapshots: Vec<(String, String, i64)>,
    pub(super) exact_total: Option<u64>,
}

impl ObservedAttempt {
    fn finish_pass(&mut self, pass: PerPassStreamingResult, divider: &AlbumDivider) {
        self.snapshots.extend(pass.deferred_album_snapshot);
        divider.mark_done(
            &pass.label,
            u64::try_from(pass.result.downloaded).unwrap_or(u64::MAX),
            pass.count,
            pass.elapsed,
        );
        self.receivers.push(PassTokenReceiver {
            pass: PassKey {
                index: pass.pass_index,
                kind: pass.kind,
                label: pass.label,
            },
            receiver: pass.token_rx,
            enumeration_complete: pass.result.enumeration_complete
                && pass.result.enumeration_errors == 0,
        });
        merge_streaming_result(&mut self.streaming, pass.result);
    }
}

impl<'a> FirstAttempt<'a> {
    pub(super) fn plan(
        passes: &'a [AlbumPass],
        config: &Arc<DownloadConfig>,
        controls: DownloadControls,
        counts: &PassCountPlan,
    ) -> Self {
        let snapshots = should_record_album_snapshots(passes, config, controls);
        let schedule = if config.requires_per_pass_paths() || snapshots {
            Schedule::PerPass {
                parallelism: passes.len().min(config.concurrent_downloads).max(1),
                deferred: deferred_unfiled_index(passes),
            }
        } else {
            let excludes = passes
                .first()
                .map(|p| Arc::clone(&p.exclude_ids))
                .unwrap_or_default();
            Schedule::Merged(if Arc::ptr_eq(&excludes, &config.exclude_asset_ids) {
                Arc::clone(config)
            } else {
                Arc::new(config.with_exclude_ids(excludes))
            })
        };
        let passes = passes
            .iter()
            .enumerate()
            .map(|(index, pass)| {
                let config = match &schedule {
                    Schedule::Merged(shared) => Arc::clone(shared),
                    Schedule::PerPass { parallelism, .. } => {
                        let mut derived = config.with_pass(pass);
                        // Replacement passes must also fit the total worker budget.
                        derived.concurrent_downloads =
                            (config.concurrent_downloads / parallelism).max(1);
                        Arc::new(derived)
                    }
                };
                PlannedPass {
                    index,
                    pass,
                    config,
                    count: counts.display_counts.get(index).copied().unwrap_or(0),
                    stream_count: counts.stream_total_counts.get(index).copied().flatten(),
                    label: if pass.album.name.is_empty() {
                        "unfiled"
                    } else {
                        &pass.album.name
                    },
                    record_snapshot: snapshots && pass.kind == PassKind::Album,
                }
            })
            .collect();
        Self {
            passes,
            schedule,
            exact_total: counts.exact_total,
        }
    }

    pub(super) async fn run(self, runner: InitialStreams<'_>) -> Result<ObservedAttempt> {
        let mut passes = self.passes;
        let mut observed = ObservedAttempt {
            streaming: StreamingResult {
                enumeration_complete: !passes.is_empty(),
                ..Default::default()
            },
            receivers: Vec::with_capacity(passes.len()),
            snapshots: Vec::new(),
            exact_total: self.exact_total,
        };
        let unfiled_only = matches!(&passes[..], [p] if p.pass.kind == PassKind::Unfiled);
        match self.schedule {
            Schedule::Merged(config) => {
                let display_total = passes.iter().map(|p| p.count).sum();
                let streams: Vec<_> = passes
                    .iter()
                    .map(|p| {
                        let (stream, receiver) = runner.open(p);
                        observed.receivers.push(PassTokenReceiver {
                            pass: PassKey {
                                index: p.index,
                                kind: p.pass.kind,
                                label: p.label.to_owned(),
                            },
                            receiver,
                            enumeration_complete: true,
                        });
                        runner.order.wrap(stream, p.index)
                    })
                    .collect();
                let mut stream: DownloadPhotoStream = Box::pin(stream::select_all(streams));
                if let [pass] = passes.as_slice()
                    && pass.pass.kind == PassKind::Unfiled
                {
                    stream = runner.heartbeat(stream, pass, "Unfiled");
                }
                observed.streaming = stream_full_inventory(
                    runner.client,
                    stream,
                    &config,
                    runner.controls,
                    display_total,
                    runner.shutdown.clone(),
                    StreamRuntime::new(None, None).deferring_metadata_drain(),
                )
                .await?;
            }
            Schedule::PerPass {
                parallelism,
                deferred,
            } => {
                let ctx = if runner.controls.run_mode.is_dry_run() {
                    None
                } else {
                    Some(preload_download_context(runner.config).await)
                };
                let labels: Vec<_> = passes.iter().map(|p| (p.label, p.count)).collect();
                let divider =
                    AlbumDivider::new(runner.controls.reporting.personality_mode, &labels);
                let deferred = deferred.map(|index| passes.remove(index));
                let members = deferred
                    .as_ref()
                    .map(|_| Arc::new(Mutex::new(FxHashSet::default())));
                let results = stream::iter(passes)
                    .map(|p| {
                        let role = match (&members, p.pass.kind) {
                            (Some(ids), PassKind::Album) => StreamRole::Collect(Arc::clone(ids)),
                            _ if unfiled_only => StreamRole::UnfiledOnly,
                            _ => StreamRole::Independent,
                        };
                        runner.run_pass(p, ctx.clone(), role)
                    })
                    .buffer_unordered(parallelism)
                    .collect::<Vec<_>>()
                    .await;
                let mut membership_complete = true;
                let mut empty_album_counts = 0u64;
                for pass in results {
                    let pass = pass?;
                    if deferred.is_some() && pass.kind == PassKind::Album {
                        if !pass.result.enumeration_complete || pass.result.enumeration_errors > 0 {
                            membership_complete = false;
                        } else if pass.result.assets_seen == 0 {
                            empty_album_counts = empty_album_counts.saturating_add(pass.count);
                        }
                    }
                    observed.finish_pass(pass, &divider);
                }
                if let (Some(p), Some(members)) = (deferred, members) {
                    if membership_complete {
                        let ids = members
                            .lock()
                            .ok()
                            .map(|ids| ids.clone())
                            .unwrap_or_default();
                        let pass = runner
                            .run_pass(p, ctx, StreamRole::DeferredUnfiled(ids))
                            .await?;
                        // Compare only the non-album records the unfiled pass actually yielded.
                        observed.exact_total = observed
                            .exact_total
                            .map(|total| total - pass.count + pass.result.assets_seen);
                        observed.finish_pass(pass, &divider);
                    } else {
                        observed.streaming.enumeration_complete = false;
                    }
                }
                observed.exact_total = observed
                    .exact_total
                    .map(|n| n.saturating_sub(empty_album_counts));
                divider.finish();
            }
        }
        Ok(observed)
    }
}

enum StreamRole {
    Independent,
    UnfiledOnly,
    Collect(Arc<Mutex<FxHashSet<String>>>),
    DeferredUnfiled(FxHashSet<String>),
}

pub(super) struct InitialStreams<'a> {
    pub(super) client: &'a Client,
    pub(super) config: &'a Arc<DownloadConfig>,
    pub(super) controls: DownloadControls,
    pub(super) shutdown: &'a CancellationToken,
    pub(super) order: &'a CompletionOrder,
    pub(super) truncated: &'a Arc<AtomicBool>,
    pub(super) strict_empty_tail_errors: bool,
}

impl InitialStreams<'_> {
    fn open(
        &self,
        p: &PlannedPass<'_>,
    ) -> (
        DownloadPhotoStream,
        tokio::sync::oneshot::Receiver<Option<String>>,
    ) {
        let (stream, rx) = open_photo_stream_for_controls(
            &p.pass.album,
            None,
            p.stream_count,
            self.config.concurrent_downloads,
            p.config.concurrent_downloads,
            self.controls,
            self.strict_empty_tail_errors,
        );
        (
            filter_stream_to_enumeration_bounds(
                stream,
                self.config,
                Arc::clone(self.truncated),
                self.shutdown.clone(),
            ),
            rx,
        )
    }

    fn heartbeat(
        &self,
        stream: DownloadPhotoStream,
        p: &PlannedPass<'_>,
        label: &'static str,
    ) -> DownloadPhotoStream {
        track_unfiled_heartbeat(
            stream,
            progress_library_label(p.pass.album.zone_name()),
            p.stream_count,
            label,
            self.shutdown.clone(),
        )
    }

    async fn run_pass(
        &self,
        p: PlannedPass<'_>,
        ctx: Option<Arc<DownloadContext>>,
        role: StreamRole,
    ) -> Result<PerPassStreamingResult> {
        let snapshot = if p.record_snapshot {
            AlbumSnapshotRecorder::start_for_pass(
                self.config.state_db.clone(),
                p.pass,
                self.config.enum_config_hash.as_deref(),
            )
            .await?
        } else {
            None
        };
        let (stream, rx) = self.open(&p);
        let stream: DownloadPhotoStream = match role {
            StreamRole::Independent => self.order.wrap(stream, p.index),
            StreamRole::UnfiledOnly => {
                self.heartbeat(self.order.wrap(stream, p.index), &p, "Unfiled")
            }
            StreamRole::Collect(ids) => {
                let stream = self.order.wrap(stream, p.index);
                Box::pin(stream.inspect(move |item| {
                    if let Ok(asset) = item
                        && let Ok(mut ids) = ids.lock()
                    {
                        ids.insert(asset.asset_record_name().to_string());
                    }
                }))
            }
            StreamRole::DeferredUnfiled(ids) => {
                let stream = self
                    .heartbeat(stream, &p, "Deferred unfiled")
                    .filter(move |item| {
                        std::future::ready(
                            item.as_ref()
                                .map_or(true, |asset| !ids.contains(asset.asset_record_name())),
                        )
                    });
                self.order.wrap(Box::pin(stream), p.index)
            }
        };
        run_full_pass_stream(
            self.client.clone(),
            stream,
            rx,
            p.config,
            FullPassStreamOptions {
                pass_index: p.index,
                controls: self.controls,
                count: p.count,
                kind: p.pass.kind,
                shutdown_token: self.shutdown.clone(),
                download_ctx: ctx,
                album_snapshot: snapshot,
            },
        )
        .await
    }
}
