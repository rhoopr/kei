use std::collections::HashMap;
use std::pin::Pin;

use serde_json::{json, Value};
use tokio_stream::Stream;
use tracing::debug;

use super::asset::PhotoAsset;
use super::queries::{encode_params, DESIRED_KEYS_VALUES};
use super::session::PhotosSession;

/// Configuration for creating a `PhotoAlbum`, bundling all non-session fields.
#[derive(Debug)]
pub struct PhotoAlbumConfig {
    pub params: HashMap<String, Value>,
    pub service_endpoint: String,
    pub name: String,
    pub list_type: String,
    pub obj_type: String,
    pub query_filter: Option<Value>,
    pub page_size: usize,
    pub zone_id: Value,
}

pub struct PhotoAlbum {
    pub(crate) name: String,
    params: HashMap<String, Value>,
    session: Box<dyn PhotosSession>,
    service_endpoint: String,
    list_type: String,
    obj_type: String,
    query_filter: Option<Value>,
    page_size: usize,
    zone_id: Value,
}

impl std::fmt::Debug for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotoAlbum")
            .field("name", &self.name)
            .field("service_endpoint", &self.service_endpoint)
            .field("list_type", &self.list_type)
            .field("obj_type", &self.obj_type)
            .field("page_size", &self.page_size)
            .finish_non_exhaustive()
    }
}

impl PhotoAlbum {
    pub fn new(config: PhotoAlbumConfig, session: Box<dyn PhotosSession>) -> Self {
        Self {
            name: config.name,
            params: config.params,
            session,
            service_endpoint: config.service_endpoint,
            list_type: config.list_type,
            obj_type: config.obj_type,
            query_filter: config.query_filter,
            page_size: config.page_size,
            zone_id: config.zone_id,
        }
    }

    /// Return total item count for this album via `HyperionIndexCountLookup`.
    pub async fn len(&self) -> anyhow::Result<u64> {
        let url = format!(
            "{}/internal/records/query/batch?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "batch": [{
                "resultsLimit": 1,
                "query": {
                    "filterBy": {
                        "fieldName": "indexCountID",
                        "fieldValue": {
                            "type": "STRING_LIST",
                            "value": [&self.obj_type]
                        },
                        "comparator": "IN",
                    },
                    "recordType": "HyperionIndexCountLookup",
                },
                "zoneWide": true,
                "zoneID": self.zone_id,
            }]
        });

        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
        )
        .await?;

        let batch: super::cloudkit::BatchQueryResponse = serde_json::from_value(response)?;
        let count = batch
            .batch
            .first()
            .and_then(|q| q.records.first())
            .and_then(|r| r.fields["itemCount"]["value"].as_u64())
            .unwrap_or(0);
        Ok(count)
    }

    /// Convenience wrapper over `photo_stream()` that collects all assets
    /// into a `Vec`. Prefer `photo_stream()` when memory is a concern.
    pub async fn photos(&self, limit: Option<u32>) -> anyhow::Result<Vec<PhotoAsset>> {
        use tokio_stream::StreamExt;
        self.photo_stream(limit)
            .collect::<Result<Vec<_>, _>>()
            .await
    }

    /// Stream photos page-by-page without buffering the full album in memory.
    ///
    /// A background tokio task drives pagination and sends assets through an
    /// `mpsc` channel whose buffer equals `page_size`. This gives natural
    /// 1-page prefetch: while the consumer processes page N, the producer
    /// is already fetching page N+1 — overlapping API latency with work.
    pub fn photo_stream(
        &self,
        limit: Option<u32>,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>> {
        let (tx, rx) = tokio::sync::mpsc::channel::<anyhow::Result<PhotoAsset>>(self.page_size);

        // The spawned task must be 'static, so clone all needed state.
        let session = self.session.clone_box();
        let service_endpoint = self.service_endpoint.clone();
        let params = self.params.clone();
        let name = self.name.clone();
        let list_type = self.list_type.clone();
        let query_filter = self.query_filter.clone();
        let page_size = self.page_size;
        let zone_id = self.zone_id.clone();

        tokio::spawn(async move {
            let mut offset: u64 = 0;
            let mut total_sent: u64 = 0;
            tracing::info!("Fetching photos from iCloud...");

            loop {
                let url = format!(
                    "{}/records/query?{}",
                    service_endpoint,
                    encode_params(&params)
                );
                let body = Self::build_list_query(
                    &list_type,
                    &query_filter,
                    page_size,
                    &zone_id,
                    offset,
                    "ASCENDING",
                );
                debug!("Album '{}' POST URL: {}", name, url);
                let response = match super::session::retry_post(
                    session.as_ref(),
                    &url,
                    &body.to_string(),
                    &[("Content-type", "text/plain")],
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                };
                debug!(
                    "Album '{}' response: {}",
                    name,
                    serde_json::to_string_pretty(&response).unwrap_or_default()
                );

                let query: super::cloudkit::QueryResponse = match serde_json::from_value(response) {
                    Ok(q) => q,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };
                let records = query.records;

                debug!(
                    "Album '{}': got {} records at offset {}",
                    name,
                    records.len(),
                    offset
                );

                let mut asset_records: HashMap<String, super::cloudkit::Record> = HashMap::new();
                let mut master_records: Vec<super::cloudkit::Record> = Vec::new();

                for rec in records {
                    debug!("  record type: {}", rec.record_type);
                    if rec.record_type == "CPLAsset" {
                        if let Some(master_id) =
                            rec.fields["masterRef"]["value"]["recordName"].as_str()
                        {
                            let master_id = master_id.to_string();
                            asset_records.insert(master_id, rec);
                        }
                    } else if rec.record_type == "CPLMaster" {
                        master_records.push(rec);
                    }
                }

                if master_records.is_empty() {
                    break;
                }

                let mut limit_reached = false;
                for master in master_records {
                    if let Some(asset_rec) = asset_records.remove(&master.record_name) {
                        let asset = PhotoAsset::from_records(master, asset_rec);
                        if tx.send(Ok(asset)).await.is_err() {
                            // Receiver dropped — consumer is done
                            return;
                        }
                        total_sent += 1;
                        if let Some(n) = limit {
                            if total_sent >= n as u64 {
                                limit_reached = true;
                                break;
                            }
                        }
                    }
                    offset += 1;
                }

                tracing::debug!(count = total_sent, "fetched photos so far");

                if limit_reached {
                    break;
                }
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }

    #[cfg(test)]
    fn list_query(&self, offset: u64, direction: &str) -> Value {
        Self::build_list_query(
            &self.list_type,
            &self.query_filter,
            self.page_size,
            &self.zone_id,
            offset,
            direction,
        )
    }

    fn build_list_query(
        list_type: &str,
        query_filter: &Option<Value>,
        page_size: usize,
        zone_id: &Value,
        offset: u64,
        direction: &str,
    ) -> Value {
        let desired_keys = &*DESIRED_KEYS_VALUES;

        let mut filter_by = vec![
            json!({
                "fieldName": "startRank",
                "fieldValue": {"type": "INT64", "value": offset},
                "comparator": "EQUALS",
            }),
            json!({
                "fieldName": "direction",
                "fieldValue": {"type": "STRING", "value": direction},
                "comparator": "EQUALS",
            }),
        ];

        if let Some(qf) = query_filter {
            if let Some(arr) = qf.as_array() {
                filter_by.extend(arr.iter().cloned());
            }
        }

        let query_part = json!({
            "filterBy": &filter_by,
            "recordType": list_type,
        });
        debug!(
            "list_query filterBy ({} items): {}",
            filter_by.len(),
            serde_json::to_string(&query_part).unwrap_or_default()
        );
        debug!(
            "list_query zoneID: {}",
            serde_json::to_string(zone_id).unwrap_or_default()
        );

        json!({
            "query": {
                "filterBy": filter_by,
                "recordType": list_type,
            },
            // CloudKit returns interleaved CPLMaster + CPLAsset records,
            // so 2 * page_size fetches page_size paired records.
            "resultsLimit": page_size * 2,
            "desiredKeys": desired_keys,
            "zoneID": zone_id,
        })
    }
}

impl std::fmt::Display for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct StubSession;

    #[async_trait::async_trait]
    impl PhotosSession for StubSession {
        async fn post(
            &self,
            _url: &str,
            _body: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            unimplemented!("stub")
        }
        async fn get(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<reqwest::Response> {
            unimplemented!("stub")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(StubSession)
        }
    }

    fn make_album(page_size: usize, query_filter: Option<Value>, zone_id: Value) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: HashMap::new(),
                service_endpoint: "https://example.com".into(),
                name: "TestAlbum".into(),
                list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter,
                page_size,
                zone_id,
            },
            Box::new(StubSession),
        )
    }

    fn default_zone() -> Value {
        json!({"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner", "zoneType": "REGULAR_CUSTOM_ZONE"})
    }

    #[test]
    fn test_list_query_ascending_offset_zero() {
        let album = make_album(200, None, default_zone());
        let q = album.list_query(0, "ASCENDING");
        let filters = q["query"]["filterBy"].as_array().unwrap();
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0]["fieldValue"]["value"], json!(0));
        assert_eq!(filters[1]["fieldValue"]["value"], "ASCENDING");
    }

    #[test]
    fn test_list_query_with_offset() {
        let album = make_album(200, None, default_zone());
        let q = album.list_query(42, "ASCENDING");
        assert_eq!(q["query"]["filterBy"][0]["fieldValue"]["value"], json!(42));
    }

    #[test]
    fn test_list_query_results_limit_double_page_size() {
        let album = make_album(100, None, default_zone());
        let q = album.list_query(0, "ASCENDING");
        assert_eq!(q["resultsLimit"], json!(200));
    }

    #[test]
    fn test_list_query_with_extra_filter() {
        let extra = json!([{"fieldName": "albumName", "comparator": "EQUALS", "fieldValue": {"type": "STRING", "value": "Favorites"}}]);
        let album = make_album(200, Some(extra), default_zone());
        let q = album.list_query(0, "ASCENDING");
        let filters = q["query"]["filterBy"].as_array().unwrap();
        assert_eq!(filters.len(), 3);
        assert_eq!(filters[2]["fieldName"], "albumName");
    }

    #[test]
    fn test_list_query_zone_id_passed_through() {
        let zone = json!({"zoneName": "CustomZone"});
        let album = make_album(200, None, zone.clone());
        let q = album.list_query(0, "ASCENDING");
        assert_eq!(q["zoneID"], zone);
    }
}
