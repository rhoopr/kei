//! Sequential CloudKit change scans and incremental event streams.

#[cfg(test)]
mod shadow_tests;

use super::lookup::ProviderRecordId;
use anyhow::Context;
use rustc_hash::FxHashSet;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
const DELETION_VALIDATION_PAGE_SIZE: u32 = 200;
use super::{ChangeStream, PhotoAlbum};
use crate::icloud::photos::asset::{ChangeEvent, DeltaRecordBuffer};
use crate::icloud::photos::cloudkit;
use crate::icloud::photos::cloudkit::ChangesZoneResponse;
use crate::icloud::photos::queries::{build_changes_zone_request, encode_params};
use crate::icloud::photos::session;
use crate::icloud::photos::session::check_changes_zone_error;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A complete, scoped page checked before pairing, callbacks or cursor acceptance.
#[must_use]
struct ValidatedChangesPage {
    zone_scope: Value,
    records: Vec<cloudkit::Record>,
    sync_token: String,
    more_coming: bool,
}

impl ValidatedChangesPage {
    fn parse(response: Value, requested_zone: &Value) -> anyhow::Result<Self> {
        let zones = response
            .get("zones")
            .and_then(Value::as_array)
            .filter(|zones| zones.len() == 1)
            .context("Invalid changes/zone cardinality")?;
        let zone = zones.first().context("Missing changes/zone result")?;
        let zone_id = zone
            .get("zoneID")
            .context("Missing changes/zone identity")?;
        let requested_name = requested_zone
            .get("zoneName")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .context("Missing requested zone identity")?;
        anyhow::ensure!(
            zone_id.get("zoneName").and_then(Value::as_str) == Some(requested_name)
                && requested_zone
                    .get("ownerRecordName")
                    .is_none_or(|owner| zone_id.get("ownerRecordName") == Some(owner)),
            "Unexpected changes/zone scope"
        );
        // Error responses can legitimately omit records and contain an empty token.
        // Preserve typed fallback classification without exposing provider values.
        if let Some(code) = zone.get("serverErrorCode").filter(|code| !code.is_null()) {
            let code = match code.as_str() {
                Some("BAD_REQUEST") => "BAD_REQUEST",
                Some("ZONE_NOT_FOUND") => "ZONE_NOT_FOUND",
                Some("RETRY_LATER") => "RETRY_LATER",
                Some("THROTTLED") => "THROTTLED",
                Some("SERVER_INTERNAL_ERROR") => "SERVER_INTERNAL_ERROR",
                _ => "UNEXPECTED_ZONE_ERROR",
            };
            check_changes_zone_error(Some(code), None, "requested zone")?;
        }
        let records = zone
            .get("records")
            .and_then(Value::as_array)
            .context("Missing or invalid changes/zone records")?;
        anyhow::ensure!(
            records
                .iter()
                .all(|record| record.get("serverErrorCode").is_none_or(Value::is_null)),
            "Changes/zone record failed"
        );
        let zone_scope = zone_id.clone();
        let response: ChangesZoneResponse = serde_json::from_value(response)
            .map_err(|_invalid_shape| anyhow::anyhow!("Invalid changes/zone page shape"))?;
        let zone = response
            .zones
            .into_iter()
            .next()
            .context("Missing changes/zone result")?;
        anyhow::ensure!(
            !zone.sync_token.trim().is_empty(),
            "Missing changes/zone successor"
        );
        Ok(Self {
            zone_scope,
            records: zone.records,
            sync_token: zone.sync_token,
            more_coming: zone.more_coming,
        })
    }

    fn observed_page(
        &self,
        body: Vec<u8>,
        scope: String,
        request_cursor: &str,
    ) -> anyhow::Result<crate::state::db::provider_inbox::ObservedPage> {
        use crate::state::db::provider_inbox::{ObservedPage, SourceIdentity};
        let response = super::super::changes_json::parse(&body)?;
        let records = response
            .get("zones")
            .and_then(Value::as_array)
            .and_then(|zones| zones.first())
            .and_then(|zone| zone.get("records"))
            .and_then(Value::as_array)
            .context("Missing capture records")?;
        for record in records {
            anyhow::ensure!(
                record
                    .get("recordName")
                    .and_then(Value::as_str)
                    .is_some_and(|name| !name.trim().is_empty()),
                "Missing provider capture source identity"
            );
            if let Some(zone) = record.get("zoneID") {
                anyhow::ensure!(
                    zone.get("zoneName") == self.zone_scope.get("zoneName")
                        && zone
                            .get("ownerRecordName")
                            .is_none_or(
                                |owner| self.zone_scope.get("ownerRecordName") == Some(owner)
                            ),
                    "Unexpected provider capture source scope"
                );
            }
        }
        anyhow::ensure!(
            self.zone_scope
                .get("ownerRecordName")
                .is_none_or(|owner| owner.as_str().is_some_and(|name| !name.trim().is_empty())),
            "Missing provider capture zone owner"
        );
        Ok(ObservedPage {
            scope,
            request_cursor: request_cursor.to_owned(),
            successor: self.sync_token.clone(),
            more_coming: self.more_coming,
            body,
            identities: self
                .records
                .iter()
                .map(|record| SourceIdentity {
                    name: record.record_name.clone(),
                    record_type: (!record.record_type.is_empty())
                        .then(|| record.record_type.clone()),
                    deleted: record.deleted.unwrap_or(false),
                })
                .collect(),
        })
    }

    fn check_continuation(&self, visited: &mut FxHashSet<String>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.more_coming || visited.insert(self.sync_token.clone()),
            "Non-progressing changes/zone continuation"
        );
        Ok(())
    }
}

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
        let mut visited = FxHashSet::default();

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

            let zone_result = ValidatedChangesPage::parse(response, &self.zone_id)?;
            zone_result.check_continuation(&mut visited)?;

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

    /// Return candidates absent from a complete raw zone delta since `sync_token`.
    /// Any source record change invalidates prior deletion evidence, including a
    /// restored record with the same share link. This never advances a checkpoint.
    /// Errors, cancellation, malformed pages and non-progressing pagination yield
    /// no evidence. IDs are inspected before media pairing or record filtering.
    pub(crate) async fn unchanged_records_since(
        &self,
        sync_token: &str,
        candidates: &[ProviderRecordId],
        shutdown_token: &CancellationToken,
    ) -> anyhow::Result<FxHashSet<ProviderRecordId>> {
        anyhow::ensure!(
            !sync_token.trim().is_empty(),
            "Missing validation checkpoint"
        );
        let mut unchanged: FxHashSet<_> = candidates.iter().cloned().collect();
        let url = format!(
            "{}/changes/zone?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let mut current_token = sync_token.to_owned();
        let mut visited = FxHashSet::from_iter([current_token.clone()]);
        loop {
            let body = build_changes_zone_request(
                &self.zone_id,
                Some(&current_token),
                DELETION_VALIDATION_PAGE_SIZE,
            )
            .to_string();
            let response = tokio::select! {
                biased;
                () = shutdown_token.cancelled() => anyhow::bail!("Source validation cancelled"),
                response = session::retry_post(self.session.as_ref(), &url, &body,
                    &[("Content-type", "text/plain")], &self.retry_config) => response?,
            };
            let zone = ValidatedChangesPage::parse(response, &self.zone_id)?;
            zone.check_continuation(&mut visited)?;
            for record in &zone.records {
                anyhow::ensure!(
                    !record.record_name.trim().is_empty(),
                    "Missing source validation record identity"
                );
                unchanged.remove(&ProviderRecordId::new(record.record_name.as_str()));
            }
            anyhow::ensure!(
                !shutdown_token.is_cancelled(),
                "Source validation cancelled"
            );
            if !zone.more_coming {
                return Ok(unchanged);
            }
            current_token = zone.sync_token;
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
        let shadow_capture = self.shadow_capture.clone();

        tokio::spawn(async move {
            let mut buffer = DeltaRecordBuffer::new();
            let mut current_token = initial_token;
            let mut visited = FxHashSet::from_iter([current_token.clone()]);

            let url = format!(
                "{}/changes/zone?{}",
                service_endpoint,
                encode_params(&params)
            );

            let stream_error: Option<anyhow::Error> = loop {
                if tx.is_closed() {
                    let _ = token_tx.send(current_token);
                    return;
                }
                let body = build_changes_zone_request(&zone_id, Some(&current_token), 200);
                tracing::debug!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    "changes/zone request"
                );

                let raw_body = match session::retry_post_changes_body(
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

                let response = match super::super::changes_json::parse(&raw_body) {
                    Ok(value) => value,
                    Err(error) => break Some(error),
                };
                let zone_result = match ValidatedChangesPage::parse(response, &zone_id) {
                    Ok(page) => page,
                    Err(error) => break Some(error),
                };
                if let Err(error) = zone_result.check_continuation(&mut visited) {
                    break Some(error);
                }

                if let Some((capture, database)) = &shadow_capture {
                    let page = capture
                        .scope(database, &zone_result.zone_scope)
                        .and_then(|scope| {
                            zone_result.observed_page(raw_body, scope, &current_token)
                        });
                    let page = match page {
                        Ok(page) => page,
                        Err(error) => break Some(error),
                    };
                    if tx.is_closed() {
                        let _ = token_tx.send(current_token);
                        return;
                    }
                    if let Err(error) = capture.capture(page).await {
                        break Some(error);
                    }
                }

                current_token = zone_result.sync_token;
                let more_coming = zone_result.more_coming;

                tracing::debug!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    records = zone_result.records.len(),
                    more_coming,
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
    use super::{CancellationToken, FxHashSet, ProviderRecordId, Value};
    use crate::icloud::photos::session::PhotosSession;
    use std::sync::{Arc, Mutex};
    #[tokio::test]
    async fn source_deletion_validation_checks_raw_ids_across_all_pages() {
        #[derive(Clone)]
        struct ValidationSession(Arc<Mutex<Vec<String>>>);
        #[async_trait::async_trait]
        impl PhotosSession for ValidationSession {
            async fn post(
                &self,
                url: &str,
                body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                assert!(url.contains("/changes/zone?"));
                let body: Value = serde_json::from_str(&body).unwrap();
                let token = body["zones"][0]["syncToken"].as_str().unwrap();
                self.0.lock().unwrap().push(token.to_owned());
                Ok(match token {
                    "saved" => canned_changes_page(
                        &[
                            changes_master("master"),
                            changes_asset("restored", "master"),
                        ],
                        "page-1",
                        true,
                    ),
                    "page-1" => canned_changes_page(
                        &[
                            json!({"recordName":"deleted","deleted":true}),
                            json!({"recordName":"unrelated","recordType":"CPLAlbum"}),
                        ],
                        "latest",
                        false,
                    ),
                    _ => panic!("unexpected validation request"),
                })
            }
            fn clone_box(&self) -> Box<dyn PhotosSession> {
                Box::new(self.clone())
            }
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let album = make_album_with_session(100, Box::new(ValidationSession(calls.clone())));
        let candidates: Vec<_> = ["absent", "restored", "deleted"]
            .into_iter()
            .map(ProviderRecordId::new)
            .collect();
        let unchanged = album
            .unchanged_records_since("saved", &candidates, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            unchanged,
            FxHashSet::from_iter([ProviderRecordId::new("absent")])
        );
        assert_eq!(*calls.lock().unwrap(), ["saved", "page-1"]);
    }
    #[tokio::test]
    async fn source_deletion_validation_rejects_incomplete_or_invalid_evidence() {
        let valid = canned_changes_page(&[], "latest", false);
        let mut bad_pages = Vec::new();
        for (key, value) in [
            ("records", Value::Null),
            ("syncToken", json!("")),
            ("moreComing", Value::Null),
            ("serverErrorCode", json!("ZONE_NOT_FOUND")),
        ] {
            let mut page = valid.clone();
            page["zones"][0][key] = value;
            bad_pages.push(page);
        }
        for field in ["zoneName", "ownerRecordName"] {
            let mut page = valid.clone();
            page["zones"][0]["zoneID"][field] = json!("wrong-zone");
            bad_pages.push(page);
        }
        bad_pages.push(canned_changes_page(
            &[json!({"recordType":"CPLAsset"})],
            "latest",
            false,
        ));
        bad_pages.push(canned_changes_page(
            &[json!({"recordName":"source","serverErrorCode":"UNKNOWN_ITEM"})],
            "latest",
            false,
        ));
        bad_pages.push(canned_changes_page(&[], "page-1", true)); // cyclic cursor
        bad_pages.push(json!({"zones":[]}));
        for page in bad_pages {
            let mock = MockPhotosSession::new()
                .ok(canned_changes_page(&[], "page-1", true))
                .ok(page);
            let album = make_album_with_session(100, Box::new(mock));
            assert!(
                album
                    .unchanged_records_since(
                        "saved",
                        &[ProviderRecordId::new("source")],
                        &CancellationToken::new()
                    )
                    .await
                    .is_err()
            );
        }
        let mock = MockPhotosSession::new()
            .ok(canned_changes_page(&[], "page-1", true))
            .err("injected tail error");
        let album = make_album_with_session(100, Box::new(mock));
        assert!(
            album
                .unchanged_records_since(
                    "saved",
                    &[ProviderRecordId::new("source")],
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(valid)));
        assert!(
            album
                .unchanged_records_since("saved", &[ProviderRecordId::new("source")], &cancelled)
                .await
                .is_err()
        );
    }
    use crate::icloud::photos::album::test_support::{
        canned_changes_page, changes_asset, changes_master, make_album_with_session,
    };
    use crate::icloud::photos::asset::ChangeEvent;
    use crate::test_helpers::{MockPhotosFlow, MockPhotosSession};
    use serde_json::json;

    #[tokio::test]
    async fn normal_changes_rejects_entire_malformed_page_before_emission() {
        use tokio_stream::StreamExt;

        let valid = canned_changes_page(
            &[
                changes_master("private-master"),
                changes_asset("private-asset", "private-master"),
            ],
            "private-successor",
            false,
        );
        let mut bad_pages = Vec::new();
        let mut missing = valid.clone();
        missing["zones"][0]
            .as_object_mut()
            .unwrap()
            .remove("records");
        bad_pages.push(missing);
        for records in [Value::Null, json!({}), json!(42)] {
            let mut page = valid.clone();
            page["zones"][0]["records"] = records;
            bad_pages.push(page);
        }
        for (key, value) in [
            ("zoneName", "private-wrong-zone"),
            ("ownerRecordName", "private-wrong-owner"),
        ] {
            let mut page = valid.clone();
            page["zones"][0]["zoneID"][key] = json!(value);
            bad_pages.push(page);
        }
        let mut missing_owner = valid.clone();
        missing_owner["zones"][0]["zoneID"]
            .as_object_mut()
            .unwrap()
            .remove("ownerRecordName");
        bad_pages.push(missing_owner);
        let mut extra_zone = valid.clone();
        extra_zone["zones"]
            .as_array_mut()
            .unwrap()
            .push(valid["zones"][0].clone());
        bad_pages.push(extra_zone);
        bad_pages.push(json!({"zones": []}));
        let mut record_error = valid.clone();
        record_error["zones"][0]["records"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "recordName": "private-error-id", "recordType": "UnsupportedType",
                "serverErrorCode": "UNKNOWN_ITEM", "reason": "private-reason"
            }));
        bad_pages.push(record_error);
        for token in [Value::Null, json!(""), json!(" "), json!(7)] {
            let mut page = valid.clone();
            page["zones"][0]["syncToken"] = token;
            bad_pages.push(page);
        }
        let mut missing_token = valid.clone();
        missing_token["zones"][0]
            .as_object_mut()
            .unwrap()
            .remove("syncToken");
        bad_pages.push(missing_token);
        let mut accepted_bad_pages = Vec::new();
        for (index, page) in bad_pages.into_iter().enumerate() {
            let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(page)));
            let (stream, token_rx) = album.changes_stream("private-saved-token");
            let items: Vec<_> = stream.collect().await;
            let token = token_rx.await.unwrap();
            if items.len() != 1 || items[0].is_ok() || token != "private-saved-token" {
                accepted_bad_pages.push(index);
            }
            for error in items.into_iter().filter_map(Result::err) {
                let diagnostic = format!("{error:#}");
                assert!(
                    !diagnostic.contains("private-"),
                    "case {index}: {diagnostic}"
                );
            }
        }
        assert!(
            accepted_bad_pages.is_empty(),
            "accepted malformed cases: {accepted_bad_pages:?}"
        );
    }

    #[tokio::test]
    async fn normal_changes_rejects_cyclic_continuation_before_page_emission() {
        use tokio_stream::StreamExt;

        for successor in ["saved", "page-1"] {
            let records = [
                changes_master("rejected"),
                changes_asset("rejected-child", "rejected"),
            ];
            let session = MockPhotosSession::new()
                .ok(canned_changes_page(&[], "page-1", true))
                .ok(canned_changes_page(&records, successor, true));
            let album = make_album_with_session(100, Box::new(session));
            let (stream, token_rx) = album.changes_stream("saved");
            let items: Vec<_> = stream.collect().await;
            assert_eq!(items.len(), 1);
            assert!(items[0].is_err());
            assert_eq!(token_rx.await.unwrap(), "page-1");
        }
    }

    #[tokio::test]
    async fn normal_changes_accepts_terminal_unchanged_token_and_unknown_records() {
        use tokio_stream::StreamExt;

        for records in [
            vec![],
            vec![json!({
                "recordName": "unknown-record", "recordType": "FutureProviderType", "fields": {"unknown": 1}
            })],
        ] {
            let album = make_album_with_session(
                100,
                Box::new(
                    MockPhotosSession::new().ok(canned_changes_page(&records, "saved", false)),
                ),
            );
            let (stream, token_rx) = album.changes_stream("saved");
            let items: Vec<_> = stream.collect().await;
            assert!(items.iter().all(Result::is_ok));
            assert_eq!(token_rx.await.unwrap(), "saved");
        }
    }

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
