//! Album count queries and same-library batching.

use super::PhotoAlbum;
use crate::icloud::photos::cloudkit;
use crate::icloud::photos::queries::encode_params;
use crate::icloud::photos::session;
use anyhow::Context;
use serde_json::{Value, json};
use std::sync::Arc;

impl PhotoAlbum {
    /// Return total item count for this album via `HyperionIndexCountLookup`.
    pub(super) async fn fetch_len(&self) -> anyhow::Result<u64> {
        let url = format!(
            "{}/internal/records/query/batch?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "batch": [Self::count_query(&self.obj_type, &self.zone_id)]
        });

        let response = session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;

        let batch: cloudkit::BatchQueryResponse = serde_json::from_value(response)
            .context("Could not read Apple's album count response")?;
        Self::count_from_query(batch.batch.first())
            .context("Could not find the album count in Apple's response")
    }

    /// Return item counts for a same-library pass set with one
    /// `/internal/records/query/batch` call. Falls back to per-album count
    /// calls if the albums do not share the same endpoint/params context.
    pub(super) async fn fetch_len_many(albums: &[&Self]) -> Vec<anyhow::Result<u64>> {
        let Some(first) = albums.first() else {
            return Vec::new();
        };
        if albums.len() == 1 {
            return vec![first.len().await];
        }
        let can_batch = albums.iter().all(|album| {
            Arc::ptr_eq(&album.service_endpoint, &first.service_endpoint)
                && Arc::ptr_eq(&album.params, &first.params)
        });
        if !can_batch {
            let mut results = Vec::with_capacity(albums.len());
            for album in albums {
                results.push(album.len().await);
            }
            return results;
        }

        let url = format!(
            "{}/internal/records/query/batch?{}",
            first.service_endpoint,
            encode_params(&first.params)
        );
        let batch: Vec<Value> = albums
            .iter()
            .map(|album| Self::count_query(&album.obj_type, &album.zone_id))
            .collect();
        let body = json!({ "batch": batch });

        let response = match session::retry_post(
            first.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &first.retry_config,
        )
        .await
        {
            Ok(response) => response,
            Err(e) => {
                tracing::debug!(target: "kei::icloud::photos::album", error = %e, "Batched album count failed; falling back to per-pass counts");
                let mut results = Vec::with_capacity(albums.len());
                for album in albums {
                    results.push(album.len().await);
                }
                return results;
            }
        };

        let batch: cloudkit::BatchQueryResponse = match serde_json::from_value(response) {
            Ok(batch) => batch,
            Err(e) => {
                tracing::debug!(target: "kei::icloud::photos::album", error = %e, "Failed to parse batched album count response; falling back to per-pass counts");
                let mut results = Vec::with_capacity(albums.len());
                for album in albums {
                    results.push(album.len().await);
                }
                return results;
            }
        };

        (0..albums.len())
            .map(|index| {
                let query = batch.batch.get(index).ok_or_else(|| {
                    anyhow::anyhow!("Apple did not return an album count for pass {index}.")
                })?;
                Self::count_from_query(Some(query)).with_context(|| {
                    format!("Could not read Apple's album count result for pass {index}")
                })
            })
            .collect()
    }

    fn count_query(obj_type: &str, zone_id: &Value) -> Value {
        json!({
            "resultsLimit": 1,
            "query": {
                "filterBy": {
                    "fieldName": "indexCountID",
                    "fieldValue": {
                        "type": "STRING_LIST",
                        "value": [obj_type]
                    },
                    "comparator": "IN",
                },
                "recordType": "HyperionIndexCountLookup",
            },
            "zoneWide": true,
            "zoneID": zone_id,
        })
    }

    fn count_from_query(query: Option<&cloudkit::QueryResponse>) -> anyhow::Result<u64> {
        let query = query.context("Apple did not return an album count query result")?;
        let record = query
            .records
            .first()
            .context("Apple's album count query returned no records")?;
        let item_count = record
            .fields
            .get("itemCount")
            .context("Apple's album count record did not include itemCount")?;
        let value = item_count
            .get("value")
            .context("Apple's album count itemCount did not include a value")?;
        value.as_u64().with_context(|| {
            format!("Apple's album count itemCount was not a non-negative integer: {value}")
        })
    }
}

#[cfg(test)]
mod tests;
