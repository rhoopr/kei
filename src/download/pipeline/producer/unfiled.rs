//! Read at most one unfiled confirmation batch ahead of the ordered producer.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use futures_util::{Stream, StreamExt, future, stream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::commands::PassKind;
use crate::download::DownloadConfig;
use crate::download::orchestration::generation::SelectionPhase;
use crate::icloud::photos::PhotoAlbum;
use crate::icloud::photos::session::is_session_error;

type Observation = Result<super::super::streaming::SelectionObservation>;
const PARTIAL_BATCH_WAIT: Duration = Duration::from_millis(250);

pub(super) fn prepare<S>(
    source: S,
    config: &DownloadConfig,
    metrics: &super::StreamProducerMetrics,
) -> (
    impl Stream<Item = Observation> + use<S>,
    impl Future<Output = ()> + use<S>,
)
where
    S: Stream<Item = Observation> + Send,
{
    let inventory = config
        .selection_run
        .as_ref()
        .zip(config.selection_pass.as_ref())
        .filter(|(run, _)| {
            run.phase == SelectionPhase::Inventory
                && config.primary_layout_pass_kind == Some(PassKind::Unfiled)
        });
    let Some((run, key)) = inventory else {
        return (
            source.left_stream(),
            future::Either::Left(future::ready(())),
        );
    };
    let run = Arc::clone(run);
    let key = key.clone();
    let auth_errors = Arc::clone(&metrics.provider_auth_errors);
    let enum_errors = Arc::clone(&metrics.enum_errors);
    let (tx, rx) = mpsc::channel::<Vec<Observation>>(1);
    let drive = async move {
        let expire = || {
            run.session_expired.store(true, Ordering::Relaxed);
            run.held.store(true, Ordering::Relaxed);
            auth_errors.fetch_add(1, Ordering::Relaxed);
            enum_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("Provider session expired during unfiled read-ahead");
        };
        // A terminal source error can follow valid observations in a partial
        // batch. Latch it when polled, before issuing any lookup for that batch.
        let source = source.take_while(|result| {
            let expired = result.as_ref().is_err_and(is_session_error);
            if expired {
                expire();
            }
            future::ready(!expired)
        });
        let groups = tokio_stream::StreamExt::chunks_timeout(
            source,
            PhotoAlbum::CONFIRMATION_BATCH_SIZE,
            PARTIAL_BATCH_WAIT,
        );
        tokio::pin!(groups);
        let fill = async {
            // Reserve before reading: a stalled consumer cannot accumulate
            // requests, observations, or signed URLs beyond this single slot.
            while let Ok(slot) = tx.reserve().await {
                let Some(group) = groups.next().await else {
                    break;
                };
                if run.session_expired.load(Ordering::Relaxed) {
                    break;
                }
                let observations: Vec<_> = group
                    .iter()
                    .filter_map(|r| r.as_ref().ok())
                    .map(|(asset, _)| asset)
                    .collect();
                let mut proofs = match run.prefetch_unfiled(&key, &observations).await {
                    Ok(proofs) => proofs,
                    Err(error) if is_session_error(&error) => {
                        expire();
                        break;
                    }
                    Err(_) => {
                        tracing::debug!("Unfiled batch unavailable; confirming individually");
                        Default::default()
                    }
                };
                slot.send(
                    group
                        .into_iter()
                        .map(|r| {
                            r.map(|(asset, proof)| {
                                let proof =
                                    proof.or_else(|| proofs.remove(asset.asset_record_name()));
                                (asset, proof)
                            })
                        })
                        .collect(),
                );
            }
        };
        tokio::select! {
            biased;
            () = tx.closed() => (),
            () = fill => (),
        }
    };
    (
        ReceiverStream::new(rx)
            .flat_map(stream::iter)
            .right_stream(),
        future::Either::Right(drive),
    )
}
