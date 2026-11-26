//! Sequential CloudKit change scans and incremental event streams.

#[cfg(test)]
mod catalog_tests;
#[cfg(test)]
mod shadow_tests;

use super::lookup::ProviderRecordId;
use anyhow::Context;
use rustc_hash::FxHashSet;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
const DELETION_VALIDATION_PAGE_SIZE: u32 = 200;
use super::{ChangeStream, PhotoAlbum};
use crate::icloud::photos::SyncTokenError;
use crate::icloud::photos::asset::{ChangeEvent, DeltaRecordBuffer};
use crate::icloud::photos::cloudkit;
use crate::icloud::photos::cloudkit::ChangesZoneResponse;
use crate::icloud::photos::error::{AcceptedReplay, ChangesBodyRejection, ReplayRefusal};
use crate::icloud::photos::hydration_progress;
use crate::icloud::photos::queries::{
    CHANGES_REPLAY_PAGE_SIZE, build_changes_zone_request, encode_params,
};
use crate::icloud::photos::session;
use crate::icloud::photos::session::check_changes_zone_error;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Only the live replay boundary can turn this scope rejection into a zone hold.
#[derive(Debug, thiserror::Error)]
enum ZoneScopeMismatch {
    #[error("Missing or invalid zones field in changes/zone response")]
    InvalidZonesField,
    #[error("Empty changes/zone results")]
    EmptyResults,
    #[error("Multiple changes/zone results")]
    MultipleResults,
    #[error("Missing or invalid changes/zone identity")]
    InvalidIdentity,
    #[error("Unexpected changes/zone scope")]
    Name,
    #[error("Unexpected changes/zone scope")]
    Owner,
    #[error("Unexpected changes/zone record scope")]
    Record,
}

#[derive(Debug, thiserror::Error)]
#[error("Missing changes/zone record identity")]
struct MissingRecordIdentity;

/// Only an initial invalid token is eligible for saved-cursor fallback.
fn classify_replay_page_refusal(
    error: &anyhow::Error,
    continuation: bool,
) -> Option<ReplayRefusal> {
    if let Some(rejection) = ChangesBodyRejection::from_shadow_error(error) {
        return Some(match rejection {
            ChangesBodyRejection::UnsuccessfulStatus => ReplayRefusal::UnsuccessfulResponse,
            ChangesBodyRejection::TooLarge => ReplayRefusal::OversizedBody,
            ChangesBodyRejection::Malformed => ReplayRefusal::MalformedJson,
            ChangesBodyRejection::DuplicateKeys => ReplayRefusal::DuplicateJsonKeys,
        });
    }
    if error.is::<MissingRecordIdentity>() {
        return Some(ReplayRefusal::InvalidRecords);
    }
    if let Some(mismatch) = error.downcast_ref::<ZoneScopeMismatch>() {
        return Some(match mismatch {
            ZoneScopeMismatch::InvalidZonesField => ReplayRefusal::InvalidZonesField,
            ZoneScopeMismatch::EmptyResults => ReplayRefusal::EmptyZoneResults,
            ZoneScopeMismatch::MultipleResults => ReplayRefusal::MultipleZoneResults,
            ZoneScopeMismatch::InvalidIdentity => ReplayRefusal::InvalidZoneIdentity,
            ZoneScopeMismatch::Name => ReplayRefusal::ZoneNameMismatch,
            ZoneScopeMismatch::Owner => ReplayRefusal::ZoneOwnerMismatch,
            ZoneScopeMismatch::Record => ReplayRefusal::RecordScopeMismatch,
        });
    }
    if let Some(
        refusal @ (ReplayRefusal::InvalidContinuationFlag
        | ReplayRefusal::InvalidReason
        | ReplayRefusal::InvalidRecords
        | ReplayRefusal::RecordError
        | ReplayRefusal::UnsuccessfulResponse
        | ReplayRefusal::MissingSuccessor),
    ) = error.downcast_ref::<ReplayRefusal>()
    {
        return Some(*refusal);
    }
    error
        .downcast_ref::<SyncTokenError>()
        .filter(|error| continuation || !matches!(error, SyncTokenError::InvalidToken { .. }))
        .map(ReplayRefusal::from_zone_error)
}

/// A complete, scoped page checked before pairing, callbacks or cursor acceptance.
#[must_use]
struct ValidatedChangesPage {
    zone_scope: Value,
    records: Vec<cloudkit::Record>,
    sync_token: String,
    more_coming: bool,
}

impl ValidatedChangesPage {
    fn parse(mut response: Value, requested_zone: &Value) -> anyhow::Result<Self> {
        anyhow::ensure!(
            response.get("serverErrorCode").is_none_or(Value::is_null),
            ReplayRefusal::UnsuccessfulResponse
        );
        let requested_name = requested_zone
            .get("zoneName")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .context("Missing requested zone identity")?;
        let zones = response
            .get_mut("zones")
            .and_then(Value::as_array_mut)
            .ok_or(ZoneScopeMismatch::InvalidZonesField)?;
        let zone = match zones.as_mut_slice() {
            [zone] => zone,
            [] => anyhow::bail!(ZoneScopeMismatch::EmptyResults),
            _ => anyhow::bail!(ZoneScopeMismatch::MultipleResults),
        };
        let zone = zone
            .as_object_mut()
            .ok_or(ZoneScopeMismatch::InvalidIdentity)?;
        let zone_id = zone
            .get("zoneID")
            .and_then(Value::as_object)
            .ok_or(ZoneScopeMismatch::InvalidIdentity)?;
        let returned_name = zone_id.get("zoneName").and_then(Value::as_str);
        anyhow::ensure!(
            returned_name == Some(requested_name),
            ZoneScopeMismatch::Name
        );
        let returned_owner = zone_id.get("ownerRecordName");
        anyhow::ensure!(
            returned_owner.is_none() || super::super::has_explicit_owner(returned_owner),
            ZoneScopeMismatch::InvalidIdentity
        );
        anyhow::ensure!(
            requested_zone
                .get("ownerRecordName")
                .is_none_or(|owner| returned_owner == Some(owner)),
            ZoneScopeMismatch::Owner
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
        let zone_scope = Value::Object(zone_id.clone());
        // Decode records separately so a malformed envelope keeps its own error.
        let Some(Value::Array(records)) = zone.remove("records") else {
            anyhow::bail!(ReplayRefusal::InvalidRecords);
        };
        anyhow::ensure!(
            records
                .iter()
                .all(|record| record.get("serverErrorCode").is_none_or(Value::is_null)),
            ReplayRefusal::RecordError
        );
        anyhow::ensure!(
            zone.get("syncToken")
                .and_then(Value::as_str)
                .is_some_and(|token| !token.trim().is_empty()),
            ReplayRefusal::MissingSuccessor
        );
        anyhow::ensure!(
            zone.get("moreComing").is_some_and(Value::is_boolean),
            ReplayRefusal::InvalidContinuationFlag
        );
        anyhow::ensure!(
            zone.get("reason")
                .is_none_or(|reason| reason.is_null() || reason.is_string()),
            ReplayRefusal::InvalidReason
        );
        let response: ChangesZoneResponse = serde_json::from_value(response)
            .map_err(|_invalid_shape| anyhow::anyhow!("Invalid changes/zone page shape"))?;
        let zone = response
            .zones
            .into_iter()
            .next()
            .context("Missing changes/zone result")?;
        for record in &records {
            anyhow::ensure!(record.is_object(), ReplayRefusal::InvalidRecords);
            if let Some(scope) = record.get("zoneID") {
                anyhow::ensure!(
                    scope.get("zoneName") == zone_scope.get("zoneName")
                        && scope
                            .get("ownerRecordName")
                            .is_none_or(|owner| zone_scope.get("ownerRecordName") == Some(owner)),
                    ZoneScopeMismatch::Record
                );
            }
        }
        let records: Vec<cloudkit::Record> = serde_json::from_value(Value::Array(records))
            .map_err(|_invalid_records| ReplayRefusal::InvalidRecords)?;
        anyhow::ensure!(
            records
                .iter()
                .all(|record| !record.record_name.trim().is_empty()),
            MissingRecordIdentity
        );
        Ok(Self {
            zone_scope,
            records,
            sync_token: zone.sync_token,
            more_coming: zone.more_coming,
        })
    }

    /// Attach the same original bytes used to validate this page.
    fn observed_page(
        &self,
        body: Vec<u8>,
        scope: String,
        request_cursor: &str,
    ) -> crate::state::db::provider_inbox::ObservedPage {
        use crate::state::db::provider_inbox::{ObservedPage, SourceIdentity};
        ObservedPage {
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
        }
    }

    fn check_continuation(&self, visited: &mut FxHashSet<String>) -> Result<(), ReplayRefusal> {
        if self.more_coming && !visited.insert(self.sync_token.clone()) {
            return Err(ReplayRefusal::RepeatedContinuation);
        }
        Ok(())
    }
}

/// Reuse the live scoped validation contract for a retained source page.
pub(crate) fn catalog_observed_page(
    body: Vec<u8>,
    scope: &str,
    request_cursor: &str,
) -> anyhow::Result<crate::state::db::provider_inbox::ObservedPage> {
    let scope_value: Value = serde_json::from_str(scope)
        .map_err(|_invalid_scope| anyhow::anyhow!("Invalid catalog source scope"))?;
    let mut zone = scope_value
        .get("zone")
        .cloned()
        .context("Missing catalog source zone")?;
    if zone.get("ownerRecordName").is_some_and(Value::is_null) {
        zone.as_object_mut()
            .context("Invalid catalog source zone")?
            .remove("ownerRecordName");
    }
    let page = ValidatedChangesPage::parse(super::super::changes_json::parse(&body)?, &zone)?;
    anyhow::ensure!(
        !request_cursor.trim().is_empty()
            && (!page.more_coming || page.sync_token != request_cursor),
        "Invalid catalog source continuation"
    );
    Ok(page.observed_page(body, scope.to_owned(), request_cursor))
}

/// Validate original album-owner membership observations before durable capture.
/// The top-level records view lets the existing source store index either query
/// shape while retaining the untouched original response bytes.
pub(super) fn validated_membership_page(request: &Value, body: &[u8]) -> anyhow::Result<Value> {
    let zones = request
        .get("zones")
        .and_then(Value::as_array)
        .filter(|zones| zones.len() == 1)
        .context("Invalid membership request scope")?;
    let zone = zones
        .first()
        .context("Missing membership request scope")?
        .get("zoneID")
        .context("Missing membership request zone")?;
    anyhow::ensure!(
        super::super::has_explicit_owner(zone.get("ownerRecordName")),
        "Unsupported membership owner"
    );
    let mut response = super::super::changes_json::parse(body)?;
    let _validated = ValidatedChangesPage::parse(response.clone(), zone)?;
    let records = response
        .pointer("/zones/0/records")
        .context("Missing membership records")?
        .clone();
    response
        .as_object_mut()
        .context("Invalid membership response")?
        .insert("records".to_owned(), records);
    Ok(response)
}

impl PhotoAlbum {
    /// Read just the compacted page at its original position. The state owner
    /// matches the body to its durable receipt before it can supply a candidate.
    pub(crate) async fn reread_replay_page(&self, cursor: &str) -> anyhow::Result<Vec<u8>> {
        let url = format!(
            "{}/changes/zone?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let request =
            build_changes_zone_request(&self.zone_id, Some(cursor), CHANGES_REPLAY_PAGE_SIZE);
        session::retry_post_changes_body(
            self.session.as_ref(),
            &url,
            &request.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await
    }
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
            hydration_progress::page_started();
            let body = build_changes_zone_request(&self.zone_id, current_token.as_deref(), 200);
            let (response, original) = if self.rank_capture.is_some() {
                let original = session::retry_post_changes_body(
                    self.session.as_ref(),
                    &url,
                    &body.to_string(),
                    &[("Content-type", "text/plain")],
                    &self.retry_config,
                )
                .await?;
                (
                    super::super::changes_json::parse(&original)?,
                    Some(original),
                )
            } else {
                (
                    session::retry_post(
                        self.session.as_ref(),
                        &url,
                        &body.to_string(),
                        &[("Content-type", "text/plain")],
                        &self.retry_config,
                    )
                    .await?,
                    None,
                )
            };

            let zone_result = ValidatedChangesPage::parse(response, &self.zone_id)?;
            zone_result.check_continuation(&mut visited)?;
            if let (Some(capture), Some(original)) = (&self.rank_capture, original) {
                capture.capture(body, original).await?;
            }

            hydration_progress::page_received(zone_result.records.len());
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
        let mut unchanged: FxHashSet<_> = candidates.iter().cloned().collect();
        self.visit_validation_changes(sync_token, shutdown_token, |record| {
            unchanged.remove(&ProviderRecordId::new(record.record_name.as_str()));
        })
        .await?;
        Ok(unchanged)
    }

    /// Observe the source boundary without applying changes or staging a cursor.
    /// Raw records include changes that produce no asset or membership event.
    pub(crate) async fn indexing_boundary(
        &self,
        cursor: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<(String, bool)> {
        let mut changed = false;
        let successor = self
            .visit_validation_changes(cursor, cancel, |_| changed = true)
            .await?;
        Ok((successor, changed))
    }

    async fn visit_validation_changes(
        &self,
        sync_token: &str,
        shutdown_token: &CancellationToken,
        mut visit: impl FnMut(&cloudkit::Record),
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            !sync_token.trim().is_empty(),
            "Missing validation checkpoint"
        );
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
                response = session::retry_post_changes_body(self.session.as_ref(), &url, &body,
                    &[("Content-type", "text/plain")], &self.retry_config) => response?,
            };
            let zone = ValidatedChangesPage::parse(
                super::super::changes_json::parse(&response)?,
                &self.zone_id,
            )?;
            zone.check_continuation(&mut visited)?;
            for record in &zone.records {
                visit(record);
            }
            anyhow::ensure!(
                !shutdown_token.is_cancelled(),
                "Source validation cancelled"
            );
            if !zone.more_coming {
                return Ok(zone.sync_token);
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
    ) -> (
        ChangeStream,
        tokio::sync::oneshot::Receiver<String>,
        tokio::sync::oneshot::Receiver<super::ChangesCompletion>,
    ) {
        let (tx, rx) = mpsc::channel::<anyhow::Result<ChangeEvent>>(200);
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        // Delivered only after terminal EOF and successful event delivery. The
        // legacy token channel also returns last-good cursors on early closure.
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();

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
            let mut records = 0usize;
            let mut accepted = Vec::new();
            let mut visited = FxHashSet::from_iter([current_token.clone()]);
            let shadow_error = |error| {
                super::super::error::classify_shadow_page_error(error, shadow_capture.is_some())
            };

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
                let body = build_changes_zone_request(
                    &zone_id,
                    Some(&current_token),
                    CHANGES_REPLAY_PAGE_SIZE,
                )
                .to_string();
                tracing::debug!(target: "kei::icloud::photos::album",
                    album = %album_name,
                    "changes/zone request"
                );

                // Receiver closure cancels read-only requests and retry delays.
                // An already-started durable shadow capture still finishes below.
                let raw_body = tokio::select! {
                    biased;
                    () = tx.closed() => {
                        let _ = token_tx.send(current_token);
                        return;
                    }
                    result = session::retry_post_changes_body(
                        session.as_ref(),
                        &url,
                        &body,
                        &[("Content-type", "text/plain")],
                        &retry_config,
                    ) => match result {
                        Ok(body) => body,
                        Err(error) => {
                            // Response validation can detect a refusal before
                            // decoding the zone. Preserve the scoped retry
                            // instead of dispatching it as a generic fallback.
                            let refusal = classify_replay_page_refusal(&error, visited.len() > 1);
                            break Some(refusal.map_or(error, ReplayRefusal::into_shadow_error));
                        }
                    },
                };

                let response = match super::super::changes_json::parse(&raw_body) {
                    Ok(value) => value,
                    Err(error) => break Some(error),
                };
                let zone_result = match ValidatedChangesPage::parse(response, &zone_id) {
                    Ok(page) => page,
                    Err(error) => {
                        // Even an empty accepted page adds to visited.
                        break Some(
                            match classify_replay_page_refusal(&error, visited.len() > 1) {
                                Some(refusal) => refusal.into_shadow_error(),
                                None => shadow_error(error),
                            },
                        );
                    }
                };
                if let Err(refusal) = zone_result.check_continuation(&mut visited) {
                    break Some(refusal.into_shadow_error());
                }

                // Unknown record types can disappear during pairing. Observe
                // the validated raw page, not the resulting ChangeEvents.
                records = records.saturating_add(zone_result.records.len());
                if let Some((capture, database)) = &shadow_capture {
                    let scope = match capture.scope(database, &zone_result.zone_scope) {
                        Ok(scope) => scope,
                        Err(error) => break Some(shadow_error(error)),
                    };
                    if tx.is_closed() {
                        let _ = token_tx.send(current_token);
                        return;
                    }
                    let page = zone_result.observed_page(raw_body, scope, &current_token);
                    match capture.capture(page).await {
                        Ok(id) => accepted.extend(id),
                        Err(error) => break Some(shadow_error(error)),
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
                let _ = tx.send(Err(AcceptedReplay::attach(e, accepted))).await;
            } else if !tx.is_closed() {
                let _ = completion_tx.send(super::ChangesCompletion {
                    records,
                    pages: accepted,
                });
            }

            let _ = token_tx.send(current_token);
        });

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            token_rx,
            completion_rx,
        )
    }
}

// Keep these tests inline so the production-source classifier excludes their
// typed-error assertions. The test paths remain changes::tests::<test_name>.
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn terminal_raw_observation_precedes_pairing_and_dropped_receiver_is_not_eof() {
        use tokio_stream::StreamExt;
        for records in [
            Vec::new(),
            vec![serde_json::json!({
                "recordName":"future-record","recordType":"FutureRankMutation","fields":{}
            })],
        ] {
            let expected_raw = !records.is_empty();
            let album = make_album_with_session(
                100,
                Box::new(MockPhotosSession::new().ok(json!({
                    "zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                        "records":records,"syncToken":"terminal","moreComing":false}]
                }))),
            );
            let (stream, token, completion) = album.changes_stream_with_completion("prior");
            assert!(stream.collect::<Vec<_>>().await.is_empty());
            assert_eq!(completion.await.unwrap().records != 0, expected_raw);
            assert_eq!(token.await.unwrap(), "terminal");
        }
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new()));
        let (stream, token, completion) = album.changes_stream_with_completion("last-good");
        drop(stream);
        assert_eq!(token.await.unwrap(), "last-good");
        assert!(completion.await.is_err());
    }

    use super::{CancellationToken, FxHashSet, ProviderRecordId, ReplayRefusal, Value};
    use crate::icloud::photos::error::{bridge_error_reason, saved_cursor_rejected};
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
        for record in [
            json!(42),
            json!(["private-positional-record", "CPLAsset", null, true]),
            json!({"recordName": "private-asset", "deleted": "private-invalid-type"}),
            json!({"recordType": "FutureProviderType"}),
            json!({"recordName": "", "recordType": "FutureProviderType"}),
            json!({"recordName": " \t", "recordType": "CPLAsset", "deleted": true}),
            json!({"recordName": null, "recordType": "CPLAsset", "deleted": true}),
        ] {
            let mut page = valid.clone();
            page["zones"][0]["records"]
                .as_array_mut()
                .unwrap()
                .push(record);
            bad_pages.push(page);
        }
        let invalid_records_cases = bad_pages.len();
        let mut missing_flag = valid.clone();
        missing_flag["zones"][0]
            .as_object_mut()
            .unwrap()
            .remove("moreComing");
        bad_pages.push(missing_flag);
        for flag in [Value::Null, json!(42), json!("private-false")] {
            let mut page = valid.clone();
            page["zones"][0]["moreComing"] = flag;
            bad_pages.push(page);
        }
        let invalid_flag_cases = invalid_records_cases..bad_pages.len();
        for reason in [json!(42), json!({"detail": "private-reason-detail"})] {
            let mut page = valid.clone();
            page["zones"][0]["reason"] = reason;
            bad_pages.push(page);
        }
        let invalid_reason_cases = invalid_flag_cases.end..bad_pages.len();
        // Foreign content cannot grant replay or saved-cursor authority.
        for (field, malformed) in [
            ("moreComing", Value::Null),
            ("reason", json!(42)),
            ("serverErrorCode", json!("FUTURE_ZONE_FAILURE")),
            ("serverErrorCode", json!("BAD_REQUEST")),
        ] {
            let mut page = valid.clone();
            page["zones"][0]["zoneID"]["ownerRecordName"] = json!("private-wrong-owner");
            page["zones"][0][field] = malformed;
            bad_pages.push(page);
        }
        let mut missing_owner = valid.clone();
        missing_owner["zones"][0]["zoneID"]
            .as_object_mut()
            .unwrap()
            .remove("ownerRecordName");
        bad_pages.push(missing_owner);
        let owner_mismatch_cases = invalid_reason_cases.end..bad_pages.len();
        for (field, malformed) in [
            ("records", Value::Null),
            ("serverErrorCode", json!("BAD_REQUEST")),
        ] {
            let mut wrong_zone = valid.clone();
            wrong_zone["zones"][0]["zoneID"]["zoneName"] = json!("private-wrong-zone");
            wrong_zone["zones"][0][field] = malformed;
            bad_pages.push(wrong_zone);
        }
        let mut missing_name = valid.clone();
        missing_name["zones"][0]["zoneID"]
            .as_object_mut()
            .unwrap()
            .remove("zoneName");
        missing_name["zones"][0]["serverErrorCode"] = json!("BAD_REQUEST");
        bad_pages.push(missing_name);
        for name in [Value::Null, json!(" "), json!(42)] {
            let mut page = valid.clone();
            page["zones"][0]["zoneID"]["zoneName"] = name;
            bad_pages.push(page);
        }
        let name_mismatch_cases = owner_mismatch_cases.end..bad_pages.len();
        let mut missing_identity = valid.clone();
        missing_identity["zones"][0]
            .as_object_mut()
            .unwrap()
            .remove("zoneID");
        missing_identity["zones"][0]["serverErrorCode"] = json!("BAD_REQUEST");
        bad_pages.push(missing_identity);
        for identity in [Value::Null, json!("private-malformed-zone"), json!([])] {
            let mut page = valid.clone();
            page["zones"][0]["zoneID"] = identity;
            bad_pages.push(page);
        }
        for entry in [
            Value::Null,
            json!("private-malformed-zone-entry"),
            json!([]),
        ] {
            bad_pages.push(json!({"zones": [entry]}));
        }
        let invalid_identity_cases = name_mismatch_cases.end..bad_pages.len();
        let empty_zones_case = bad_pages.len();
        bad_pages.push(json!({
            "zones": [], "syncToken": "private-unscoped-successor", "moreComing": false
        }));
        let multiple_zones_case = bad_pages.len();
        let mut extra_zone = valid.clone();
        extra_zone["zones"]
            .as_array_mut()
            .unwrap()
            .push(valid["zones"][0].clone());
        bad_pages.push(extra_zone);
        bad_pages.extend([
            json!({"zones": null, "syncToken": "private-unscoped-successor"}),
            json!({}),
            json!({"zones": {"zoneName": "private-non-array"}}),
            Value::Null,
            json!([{"zones": []}]),
            json!("private-non-object-response"),
        ]);
        let invalid_zones_cases = multiple_zones_case + 1..bad_pages.len();
        for scope in [
            json!({"zoneName": "private-wrong-record-zone"}),
            json!({"zoneName": "PrimarySync", "ownerRecordName": "private-wrong-record-owner"}),
        ] {
            let mut page = valid.clone();
            page["zones"][0]["records"][1]["zoneID"] = scope;
            bad_pages.push(page);
        }
        let record_scope_cases = invalid_zones_cases.end..bad_pages.len();
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
                if index < invalid_records_cases {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::InvalidRecords)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if invalid_flag_cases.contains(&index) {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::InvalidContinuationFlag)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if invalid_reason_cases.contains(&index) {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::InvalidReason)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if owner_mismatch_cases.contains(&index) {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::ZoneOwnerMismatch)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if name_mismatch_cases.contains(&index) {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::ZoneNameMismatch)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if invalid_identity_cases.contains(&index) {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::InvalidZoneIdentity)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if index == empty_zones_case {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::EmptyZoneResults)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if index == multiple_zones_case {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::MultipleZoneResults)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if invalid_zones_cases.contains(&index) {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::InvalidZonesField)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                } else if record_scope_cases.contains(&index) {
                    assert!(matches!(
                        ReplayRefusal::from_shadow_error(&error),
                        Some(ReplayRefusal::RecordScopeMismatch)
                    ));
                    assert!(!saved_cursor_rejected(&error));
                }
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

    #[test]
    fn stored_page_scope_mismatch_is_not_a_live_replay_refusal() {
        let valid = canned_changes_page(&[], "next", false);
        let scope = json!({"zone": valid["zones"][0]["zoneID"].clone()}).to_string();
        let assert_not_live = |page: Value| {
            let error =
                super::catalog_observed_page(serde_json::to_vec(&page).unwrap(), &scope, "saved")
                    .unwrap_err();
            assert!(error.is::<super::ZoneScopeMismatch>());
            let error = crate::icloud::photos::error::classify_shadow_page_error(error, true);
            assert!(ReplayRefusal::from_shadow_error(&error).is_none());
            assert!(!format!("{error:#}").contains("private-foreign-scope"));
        };
        for (field, value) in [
            ("zoneName", Some("private-foreign-scope")),
            ("zoneName", None),
            ("ownerRecordName", Some("private-foreign-scope")),
            ("zoneID", None),
        ] {
            let mut page = valid.clone();
            let entry = &mut page["zones"][0];
            let fields = if field == "zoneID" {
                entry.as_object_mut().unwrap()
            } else {
                entry["zoneID"].as_object_mut().unwrap()
            };
            if let Some(value) = value {
                fields.insert(field.to_owned(), json!(value));
            } else {
                fields.remove(field);
            }
            assert_not_live(page);
        }
        assert_not_live(json!({"zones": []}));
        assert_not_live(json!({"zones": null}));
        assert_not_live(json!({"zones": [null]}));
        assert_not_live(json!({}));
        assert_not_live(Value::Null);
        let entry = &valid["zones"][0];
        assert_not_live(json!({"zones": [entry, entry]}));
        assert_not_live(canned_changes_page(
            &[json!({"recordName": "record", "zoneID": {"zoneName": "private-foreign-scope"}})],
            "next",
            false,
        ));
    }

    #[test]
    fn returned_owner_is_optional_for_an_ownerless_request_but_must_be_valid_if_present() {
        let requested_zone = json!({"zoneName": "PrimarySync"});
        let mut page = canned_changes_page(&[], "next", false);
        assert!(super::ValidatedChangesPage::parse(page.clone(), &requested_zone).is_ok());
        page["zones"][0]["zoneID"]
            .as_object_mut()
            .unwrap()
            .remove("ownerRecordName");
        assert!(super::ValidatedChangesPage::parse(page.clone(), &requested_zone).is_ok());

        // A malformed owner cannot let BAD_REQUEST expire the saved cursor.
        page["zones"][0]["serverErrorCode"] = json!("BAD_REQUEST");
        for owner in [Value::Null, json!(""), json!(" \t"), json!(42), json!({})] {
            page["zones"][0]["zoneID"]["ownerRecordName"] = owner;
            let error = super::ValidatedChangesPage::parse(page.clone(), &requested_zone)
                .err()
                .expect("malformed owner must reject the response");
            assert!(matches!(
                error.downcast_ref::<super::ZoneScopeMismatch>(),
                Some(super::ZoneScopeMismatch::InvalidIdentity)
            ));
        }
    }

    #[test]
    fn stored_page_missing_record_identity_is_not_a_live_replay_refusal() {
        let page = canned_changes_page(
            &[json!({"recordName": " \t", "deleted": true})],
            "next",
            false,
        );
        let scope = json!({"zone": page["zones"][0]["zoneID"].clone()}).to_string();
        let error =
            super::catalog_observed_page(serde_json::to_vec(&page).unwrap(), &scope, "saved")
                .unwrap_err();
        assert!(error.is::<super::MissingRecordIdentity>());
        let error = crate::icloud::photos::error::classify_shadow_page_error(error, true);
        assert!(ReplayRefusal::from_shadow_error(&error).is_none());
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

        for (records, reason) in [
            (vec![], None),
            (vec![], Some(Value::Null)),
            (
                vec![json!({
                    "recordName": "unknown-record", "recordType": "FutureProviderType",
                    "zoneID": {"zoneName": "PrimarySync"},
                    "fields": {"reference": {"value": {"zoneID": {"zoneName": "OtherZone"}}}}
                })],
                Some(json!("private-reason-text")),
            ),
        ] {
            let mut page = canned_changes_page(&records, "saved", false);
            if let Some(reason) = reason {
                page["zones"][0]["reason"] = reason;
            }
            let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(page)));
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
        assert!(matches!(
            ReplayRefusal::from_shadow_error(&errors[0]),
            Some(ReplayRefusal::RejectedContinuation)
        ));
        assert!(!saved_cursor_rejected(&errors[0]));
        assert_eq!(bridge_error_reason(&errors[0]), "continuation_rejected");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "token-page2",
            "should preserve last-good token from page 2, not initial or error page"
        );
    }

    #[tokio::test]
    async fn scoped_replay_refusals_are_not_saved_cursor_expiry() {
        use tokio_stream::StreamExt;

        for (code, reason, after_empty_page) in [
            ("BAD_REQUEST", "continuation_rejected", true),
            ("ZONE_NOT_FOUND", "zone_not_found", true),
            ("ZONE_NOT_FOUND", "zone_not_found", false),
            ("RETRY_LATER", "retry_later", true),
            ("RETRY_LATER", "retry_later", false),
            ("SERVER_INTERNAL_ERROR", "server_error", true),
            ("SERVER_INTERNAL_ERROR", "server_error", false),
            ("THROTTLED", "throttled", true),
            ("THROTTLED", "throttled", false),
            ("FUTURE_ZONE_FAILURE", "unrecognized_zone_error", true),
            ("FUTURE_ZONE_FAILURE", "unrecognized_zone_error", false),
        ] {
            let mut mock = MockPhotosSession::new();
            if after_empty_page {
                mock = mock.ok(canned_changes_page(&[], "continuation", true));
            }
            let mock = mock.ok(json!({"zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "serverErrorCode": code,
                "reason": {"detail": "private-provider-detail"}
            }]}));
            let album = make_album_with_session(100, Box::new(mock));
            let (mut stream, _last_token, completion) =
                album.changes_stream_with_completion("saved");
            let error = stream.next().await.unwrap().unwrap_err();
            assert!(ReplayRefusal::from_shadow_error(&error).is_some(), "{code}");
            assert_eq!(bridge_error_reason(&error), reason);
            assert!(!saved_cursor_rejected(&error), "{code}");
            assert!(completion.await.is_err(), "{code}");
            let diagnostic = format!("{error:#}");
            assert!(!diagnostic.contains(code));
            assert!(!diagnostic.contains("private-provider-detail"));
        }
    }

    #[tokio::test]
    async fn unchanged_terminal_cursor_is_valid_but_repeated_continuation_is_refused() {
        use tokio_stream::StreamExt;

        for more_coming in [false, true] {
            let records = if more_coming {
                vec![
                    json!({"recordName": "rejected-delete", "recordType": "CPLAsset", "deleted": true}),
                ]
            } else {
                Vec::new()
            };
            let mock =
                MockPhotosSession::new().ok(canned_changes_page(&records, "saved", more_coming));
            let album = make_album_with_session(100, Box::new(mock));
            let (mut stream, token, completion) = album.changes_stream_with_completion("saved");
            if more_coming {
                let error = stream.next().await.unwrap().unwrap_err();
                assert!(matches!(
                    ReplayRefusal::from_shadow_error(&error),
                    Some(ReplayRefusal::RepeatedContinuation)
                ));
                assert!(!saved_cursor_rejected(&error));
            }
            assert!(stream.next().await.is_none());
            assert_eq!(token.await.unwrap(), "saved");
            assert_eq!(
                completion.await.ok().map(|c| c.records),
                (!more_coming).then_some(0)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn response_wide_errors_keep_transport_retry_classification() {
        use crate::icloud::photos::session::CloudKitServerError;
        use crate::retry::RetryConfig;
        use tokio_stream::StreamExt;

        let max_retries = RetryConfig::default().max_retries;
        for (code, retries, after_empty_page) in [
            ("RETRY_LATER", max_retries, true),
            ("RETRY_LATER", max_retries, false),
            ("THROTTLED", max_retries, true),
            ("THROTTLED", max_retries, false),
            ("SERVER_INTERNAL_ERROR", 0, true),
            ("SERVER_INTERNAL_ERROR", 0, false),
            ("ZONE_NOT_FOUND", 0, false),
            ("FUTURE_ZONE_FAILURE", 0, true),
            ("FUTURE_ZONE_FAILURE", 0, false),
        ] {
            let mut mock = MockPhotosSession::new();
            if after_empty_page {
                mock = mock.ok(canned_changes_page(&[], "continuation", true));
            }
            for attempt in 0..=retries {
                mock = mock.ok(json!({"serverErrorCode": code, "reason": attempt.to_string()}));
            }
            // This would succeed if the transport retried one time too many.
            let mock = mock.ok(canned_changes_page(&[], "unexpected", false));
            let album = make_album_with_session(100, Box::new(mock));
            let (mut stream, _last_token, completion) =
                album.changes_stream_with_completion("saved");
            let error = stream.next().await.unwrap().unwrap_err();
            let server_error = error.downcast_ref::<CloudKitServerError>().unwrap();
            assert_eq!(server_error.code.as_ref(), code);
            assert_eq!(
                server_error.retryable,
                matches!(code, "RETRY_LATER" | "THROTTLED")
            );
            assert_eq!(server_error.service_not_activated, code == "ZONE_NOT_FOUND");
            assert_eq!(server_error.reason.parse::<u32>().unwrap(), retries);
            assert!(ReplayRefusal::from_shadow_error(&error).is_none());
            assert!(completion.await.is_err());
        }
    }

    #[tokio::test]
    async fn http_503_after_an_accepted_page_stays_a_transport_error() {
        assert_http_replay_error(503, "SERVER_INTERNAL_ERROR", Some("continuation")).await;
        assert_http_replay_error(503, "FUTURE_ZONE_FAILURE", Some("continuation")).await;
    }

    #[tokio::test]
    async fn http_503_on_first_replay_stays_a_transport_error() {
        assert_http_replay_error(503, "SERVER_INTERNAL_ERROR", None).await;
    }

    #[tokio::test]
    async fn http_429_after_an_accepted_page_stays_a_transport_error() {
        assert_http_replay_error(429, "THROTTLED", Some("continuation")).await;
    }

    #[tokio::test]
    async fn http_429_on_first_replay_stays_a_transport_error() {
        assert_http_replay_error(429, "THROTTLED", None).await;
    }

    async fn assert_http_replay_error(status: u16, code: &str, continuation: Option<&str>) {
        use crate::icloud::photos::session::HttpStatusError;
        use crate::retry::RetryConfig;
        use std::time::Duration;
        use tokio_stream::StreamExt;
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        if let Some(cursor) = continuation {
            let accepted = canned_changes_page(&[], cursor, true);
            Mock::given(method("POST"))
                .and(path("/changes/zone"))
                .and(body_string_contains(r#""syncToken":"saved""#))
                .respond_with(ResponseTemplate::new(200).set_body_json(accepted))
                .expect(1)
                .mount(&server)
                .await;
        }
        let cursor = continuation.unwrap_or("saved");
        let refused = json!({"zones": [{
            "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
            "serverErrorCode": code
        }]});
        Mock::given(method("POST"))
            .and(path("/changes/zone"))
            .and(body_string_contains(format!(r#""syncToken":"{cursor}""#)))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Retry-After", "5")
                    .set_body_json(refused),
            )
            .expect(3)
            .mount(&server)
            .await;
        let mut album = make_album_with_session(100, Box::new(reqwest::Client::new()));
        album.service_endpoint = Arc::from(server.uri());
        album.retry_config = RetryConfig {
            max_retries: 2,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let (mut stream, _last_token, completion) = album.changes_stream_with_completion("saved");
        let error = stream.next().await.unwrap().unwrap_err();
        let http = error.downcast_ref::<HttpStatusError>().unwrap();
        assert_eq!(http.status, status);
        assert_eq!(http.retry_after, Some(Duration::from_secs(5)));
        assert!(ReplayRefusal::from_shadow_error(&error).is_none());
        assert!(completion.await.is_err());
        server.verify().await;
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
        assert!(saved_cursor_rejected(&err));

        match sync_err {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(&**reason, "Unknown sync continuation type");
            }
            other => panic!("expected InvalidToken variant, got: {other:?}"),
        }
    }
}
