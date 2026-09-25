//! Sequential CloudKit change scans and incremental event streams.

use super::{ChangeStream, PhotoAlbum};
use crate::icloud::photos::asset::{ChangeEvent, DeltaRecordBuffer};
use crate::icloud::photos::cloudkit;
use crate::icloud::photos::cloudkit::ChangesZoneResponse;
use crate::icloud::photos::queries::{build_changes_zone_request, encode_params};
use crate::icloud::photos::session;
use crate::icloud::photos::session::check_changes_zone_error;
use std::sync::Arc;
use tokio::sync::mpsc;

impl PhotoAlbum {
    pub(super) async fn scan_changes_zone<F>(&self, mut on_record: F) -> anyhow::Result<()>
    where
        F: FnMut(cloudkit::Record) -> bool,
    {
        let url = format!(
            "{}/changes/zone?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let mut current_token: Option<String> = None;

        loop {
            let body = build_changes_zone_request(&self.zone_id, current_token.as_deref(), 200);
            let response = session::retry_post(
                self.session.as_ref(),
                &url,
                &body.to_string(),
                &[("Content-type", "text/plain")],
                &self.retry_config,
            )
            .await?;

            let changes_resp: ChangesZoneResponse = serde_json::from_value(response)?;
            let Some(zone_result) = changes_resp.zones.into_iter().next() else {
                anyhow::bail!("Apple changes/zone returned no zones.");
            };
            let zone_name = zone_result.zone_id.zone_name.clone();
            check_changes_zone_error(
                zone_result.server_error_code.as_deref(),
                zone_result.reason.as_deref(),
                &zone_name,
            )?;

            current_token = Some(zone_result.sync_token);
            let more_coming = zone_result.more_coming;
            for record in zone_result.records {
                if !on_record(record) {
                    return Ok(());
                }
            }
            if !more_coming {
                return Ok(());
            }
        }
    }

    /// Stream record changes since the given syncToken via `changes/zone`.
    ///
    /// Returns a stream of `ChangeEvent`s and a oneshot receiver for the final syncToken.
    /// The syncToken is sent through the oneshot after all pages have been consumed
    /// (moreComing: false), or on error with the last successfully consumed token.
    ///
    /// This method is inherently sequential -- each page's syncToken feeds the next request.
    /// No parallel fetchers.
    pub(super) fn stream_changes(
        &self,
        sync_token: &str,
    ) -> (ChangeStream, tokio::sync::oneshot::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<anyhow::Result<ChangeEvent>>(200);
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();

        let session = self.session.clone_box();
        let service_endpoint = Arc::clone(&self.service_endpoint);
        let params = Arc::clone(&self.params);
        let zone_id = Arc::clone(&self.zone_id);
        let initial_token = sync_token.to_string();
        let album_name = Arc::clone(&self.name);
        let retry_config = self.retry_config;

        tokio::spawn(async move {
            let mut buffer = DeltaRecordBuffer::new();
            let mut current_token = initial_token;

            let url = format!(
                "{}/changes/zone?{}",
                service_endpoint,
                encode_params(&params)
            );

            let stream_error: Option<anyhow::Error> = loop {
                let body = build_changes_zone_request(&zone_id, Some(&current_token), 200);
                tracing::debug!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    token = %current_token,
                    "changes/zone request"
                );

                let response = match session::retry_post(
                    session.as_ref(),
                    &url,
                    &body.to_string(),
                    &[("Content-type", "text/plain")],
                    &retry_config,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => break Some(e),
                };

                let changes_resp: ChangesZoneResponse = match serde_json::from_value(response) {
                    Ok(r) => r,
                    Err(e) => break Some(e.into()),
                };

                let Some(zone_result) = changes_resp.zones.into_iter().next() else {
                    break Some(anyhow::anyhow!("Apple changes/zone returned no zones."));
                };

                // Check for zone-level errors BEFORE advancing current_token.
                // On any zone error (including transient RETRY_LATER), the loop
                // breaks with current_token still set to the last-known-good
                // value so the caller can retry from a valid checkpoint.
                let zone_name = zone_result.zone_id.zone_name.clone();
                if let Err(sync_err) = check_changes_zone_error(
                    zone_result.server_error_code.as_deref(),
                    zone_result.reason.as_deref(),
                    &zone_name,
                ) {
                    break Some(sync_err.into());
                }

                current_token = zone_result.sync_token;
                let more_coming = zone_result.more_coming;

                tracing::debug!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    records = zone_result.records.len(),
                    more_coming,
                    new_token = %current_token,
                    "changes/zone page received"
                );

                let events = buffer.process_records(zone_result.records);
                for event in events {
                    if tx.send(Ok(event)).await.is_err() {
                        // Receiver dropped -- no one to flush to
                        let _ = token_tx.send(current_token);
                        return;
                    }
                }

                if !more_coming {
                    break None;
                }
            };

            // Always flush unpaired records, even on error
            let flush_events = buffer.flush();
            if stream_error.is_some() && !flush_events.is_empty() {
                tracing::warn!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    orphaned = flush_events.len(),
                    "flushing unpaired records after stream error"
                );
            }
            for event in flush_events {
                if tx.send(Ok(event)).await.is_err() {
                    let _ = token_tx.send(current_token);
                    return;
                }
            }

            if let Some(e) = stream_error {
                let _ = tx.send(Err(e)).await;
            }

            let _ = token_tx.send(current_token);
        });

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            token_rx,
        )
    }
}

// Keep these tests inline so the production-source classifier excludes their
// typed-error assertions. The test paths remain changes::tests::<test_name>.
#[cfg(test)]
mod tests {
    use crate::icloud::photos::album::test_support::{
        canned_changes_page, changes_asset, changes_master, make_album_with_session,
    };
    use crate::icloud::photos::asset::ChangeEvent;
    use crate::test_helpers::{MockPhotosFlow, MockPhotosSession};
    use serde_json::json;

    #[tokio::test]
    async fn offline_replay_incremental_changes_fixture() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_photo_page("master-replay-change", "token-incremental", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);
        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("change event"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "master-replay-change");
        assert!(events[0].asset.is_some());
        assert_eq!(
            token_rx.await.expect("sync token sender"),
            "token-incremental"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_single_page() {
        use tokio_stream::StreamExt;

        let records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let mock = MockPhotosFlow::new()
            .changes_zone_page(records, "token-final", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "master-1");
        assert!(events[0].asset.is_some());
        assert_eq!(events[0].record_type.as_deref(), Some("CPLMaster"));

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-final");
    }

    #[tokio::test]
    async fn test_changes_stream_multiple_pages() {
        use tokio_stream::StreamExt;

        let page1_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let page2_records = vec![
            changes_master("master-2"),
            changes_asset("asset-2", "master-2"),
        ];
        let mock = MockPhotosFlow::new()
            .changes_zone_page(page1_records, "token-page1", true)
            .changes_zone_page(page2_records, "token-page2", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 2);
        assert_eq!(&*events[0].record_name, "master-1");
        assert_eq!(&*events[1].record_name, "master-2");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-page2");
    }

    #[tokio::test]
    async fn test_changes_stream_empty_page_continues() {
        use tokio_stream::StreamExt;

        // First page: empty records but moreComing: true (normal API behavior)
        // Second page: actual records, moreComing: false
        let page2_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let mock = MockPhotosFlow::new()
            .changes_zone_page(Vec::new(), "token-empty", true)
            .changes_zone_page(page2_records, "token-final", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1, "should yield the event from page 2");
        assert_eq!(&*events[0].record_name, "master-1");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-final");
    }

    #[tokio::test]
    async fn test_changes_stream_zone_error() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_zone_error("BAD_REQUEST", "Unknown sync continuation type", "")
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("bad-token");
        tokio::pin!(stream);

        let mut items: Vec<anyhow::Result<ChangeEvent>> = Vec::new();
        while let Some(result) = stream.next().await {
            items.push(result);
        }

        assert_eq!(items.len(), 1, "should have exactly one error item");
        let err = items.into_iter().next().expect("should have item");
        assert!(err.is_err());
        let err_msg = format!("{}", err.unwrap_err());
        assert!(
            err_msg.contains("sync token is no longer valid"),
            "error should mention invalid sync token, got: {err_msg}"
        );

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "bad-token",
            "on error, should preserve the last-good token for checkpoint"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_transient_zone_error_preserves_initial_token() {
        // A transient zone code (RETRY_LATER, SERVER_INTERNAL_ERROR, etc.)
        // on the very first page must not lose the caller's initial sync_token.
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_zone_error("RETRY_LATER", "temporary backend issue", "")
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-T0");
        tokio::pin!(stream);

        let mut errors = 0usize;
        while let Some(result) = stream.next().await {
            if result.is_err() {
                errors += 1;
            }
        }
        assert_eq!(errors, 1, "should surface the zone error");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "token-T0",
            "transient zone error on first page must preserve the caller's initial token"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_mid_stream_error_preserves_last_good_token() {
        use tokio_stream::StreamExt;

        let page1_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let page2_records = vec![
            changes_master("master-2"),
            changes_asset("asset-2", "master-2"),
        ];
        // Pages 1-2 succeed, page 3 returns a zone error
        let mock = MockPhotosSession::new()
            .ok(canned_changes_page(&page1_records, "token-page1", true))
            .ok(canned_changes_page(&page2_records, "token-page2", true))
            .ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "",
                    "moreComing": false,
                    "serverErrorCode": "BAD_REQUEST",
                    "reason": "Unknown sync continuation type"
                }]
            }));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => events.push(event),
                Err(e) => errors.push(e),
            }
        }

        assert_eq!(events.len(), 2, "should have events from pages 1 and 2");
        assert_eq!(&*events[0].record_name, "master-1");
        assert_eq!(&*events[1].record_name, "master-2");
        assert_eq!(errors.len(), 1, "should have exactly one error from page 3");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "token-page2",
            "should preserve last-good token from page 2, not initial or error page"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_hard_deleted_record() {
        use crate::icloud::photos::types::ChangeReason;
        use tokio_stream::StreamExt;

        let records = vec![json!({
            "recordName": "deleted-record-1",
            "recordType": null,
            "deleted": true,
            "recordChangeTag": "ct-del"
        })];
        let mock =
            MockPhotosSession::new().ok(canned_changes_page(&records, "token-after-delete", false));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "deleted-record-1");
        assert_eq!(events[0].reason, ChangeReason::HardDeleted);
        assert!(events[0].asset.is_none(), "hard-deleted has no asset");
        assert!(
            events[0].record_type.is_none(),
            "hard-deleted has no record type"
        );

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-after-delete");
    }

    #[tokio::test]
    async fn test_changes_stream_invalid_token_yields_typed_error() {
        use crate::icloud::photos::session::SyncTokenError;
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new().ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "",
                "moreComing": false,
                "serverErrorCode": "BAD_REQUEST",
                "reason": "Unknown sync continuation type"
            }]
        }));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, _token_rx) = album.changes_stream("old-token");
        tokio::pin!(stream);

        let mut items: Vec<anyhow::Result<ChangeEvent>> = Vec::new();
        while let Some(result) = stream.next().await {
            items.push(result);
        }

        assert_eq!(items.len(), 1, "should have exactly one error item");
        let err = items
            .into_iter()
            .next()
            .expect("should have item")
            .expect_err("should be an error");

        let sync_err = err
            .downcast_ref::<SyncTokenError>()
            .expect("error should downcast to SyncTokenError");

        match sync_err {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(&**reason, "Unknown sync continuation type");
            }
            other => panic!("expected InvalidToken variant, got: {other:?}"),
        }
    }
}
