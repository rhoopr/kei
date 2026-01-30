use std::collections::HashMap;

use serde_json::{json, Value};
use tracing::debug;

use super::asset::PhotoAsset;
use super::queries::{encode_params, DESIRED_KEYS_VALUES};
use super::session::PhotosSession;

#[allow(dead_code)]
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

impl PhotoAlbum {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        params: HashMap<String, Value>,
        session: Box<dyn PhotosSession>,
        service_endpoint: String,
        name: String,
        list_type: String,
        obj_type: String,
        query_filter: Option<Value>,
        page_size: usize,
        zone_id: Value,
    ) -> Self {
        Self {
            name,
            params,
            session,
            service_endpoint,
            list_type,
            obj_type,
            query_filter,
            page_size,
            zone_id,
        }
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return total item count for this album via `HyperionIndexCountLookup`.
    #[allow(dead_code)]
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

        let response = self
            .session
            .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
            .await?;

        let count = response["batch"][0]["records"][0]["fields"]["itemCount"]["value"]
            .as_u64()
            .unwrap_or(0);
        Ok(count)
    }

    /// Fetch all photos in this album, handling pagination.
    pub async fn photos(&self) -> anyhow::Result<Vec<PhotoAsset>> {
        let mut all_assets: Vec<PhotoAsset> = Vec::new();
        let mut offset: u64 = 0;

        loop {
            let url = format!(
                "{}/records/query?{}",
                self.service_endpoint,
                encode_params(&self.params)
            );
            let body = self.list_query(offset);
            debug!("Album '{}' POST URL: {}", self.name, url);
            let response = self
                .session
                .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
                .await?;
            debug!("Album '{}' response: {}", self.name, serde_json::to_string_pretty(&response).unwrap_or_default());

            let mut response = response;
            let records = match response.get_mut("records").and_then(|v| v.as_array_mut()) {
                Some(r) => std::mem::take(r),
                None => {
                    debug!("No 'records' field in response for album '{}'", self.name);
                    break;
                }
            };

            debug!(
                "Album '{}': got {} records at offset {}",
                self.name,
                records.len(),
                offset
            );

            let mut asset_records: HashMap<String, Value> = HashMap::new();
            let mut master_records: Vec<Value> = Vec::new();

            for mut rec in records {
                let record_type = rec["recordType"].as_str().unwrap_or_else(|| {
                    tracing::warn!("Missing expected field: recordType");
                    ""
                });
                debug!("  record type: {}", record_type);
                if record_type == "CPLAsset" {
                    if let Some(master_id) =
                        rec["fields"]["masterRef"]["value"]["recordName"].as_str()
                    {
                        let master_id = master_id.to_string();
                        asset_records.insert(master_id, std::mem::take(&mut rec));
                    }
                } else if record_type == "CPLMaster" {
                    master_records.push(std::mem::take(&mut rec));
                }
            }

            if master_records.is_empty() {
                break;
            }

            for master in master_records {
                let record_name = master["recordName"].as_str().unwrap_or_else(|| {
                    tracing::warn!("Missing expected field: master recordName");
                    ""
                });
                if let Some(asset_rec) = asset_records.remove(record_name) {
                    all_assets.push(PhotoAsset::new(master, asset_rec));
                } else {
                    offset += 1;
                    continue;
                }
                offset += 1;
            }
        }

        Ok(all_assets)
    }

    fn list_query(&self, offset: u64) -> Value {
        let desired_keys = &*DESIRED_KEYS_VALUES;

        let mut filter_by = vec![
            json!({
                "fieldName": "startRank",
                "fieldValue": {"type": "INT64", "value": offset},
                "comparator": "EQUALS",
            }),
            json!({
                "fieldName": "direction",
                "fieldValue": {"type": "STRING", "value": "ASCENDING"},
                "comparator": "EQUALS",
            }),
        ];

        if let Some(ref qf) = self.query_filter {
            if let Some(arr) = qf.as_array() {
                filter_by.extend(arr.iter().cloned());
            }
        }

        let query_part = json!({
            "filterBy": &filter_by,
            "recordType": &self.list_type,
        });
        debug!("list_query filterBy ({} items): {}", filter_by.len(), serde_json::to_string(&query_part).unwrap_or_default());
        debug!("list_query zoneID: {}", serde_json::to_string(&self.zone_id).unwrap_or_default());

        json!({
            "query": {
                "filterBy": filter_by,
                "recordType": &self.list_type,
            },
            "resultsLimit": self.page_size * 2,
            "desiredKeys": desired_keys,
            "zoneID": self.zone_id,
        })
    }
}

impl std::fmt::Display for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl std::fmt::Debug for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<PhotoAlbum: '{}'>", self.name)
    }
}
