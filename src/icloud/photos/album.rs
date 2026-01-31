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

    /// Fetch all photos in this album, handling pagination.
    /// If `limit` is `Some(n)`, stops after `n` photos.
    pub async fn photos(&self, limit: Option<u32>) -> anyhow::Result<Vec<PhotoAsset>> {
        self.fetch_photos("ASCENDING", limit).await
    }

    async fn fetch_photos(&self, direction: &str, limit: Option<u32>) -> anyhow::Result<Vec<PhotoAsset>> {
        let mut all_assets: Vec<PhotoAsset> = Vec::new();
        let mut offset: u64 = 0;
        tracing::info!("Fetching photos from iCloud...");

        loop {
            let url = format!(
                "{}/records/query?{}",
                self.service_endpoint,
                encode_params(&self.params)
            );
            let body = self.list_query(offset, direction);
            debug!("Album '{}' POST URL: {}", self.name, url);
            let response = super::session::retry_post(
                self.session.as_ref(),
                &url,
                &body.to_string(),
                &[("Content-type", "text/plain")],
            )
            .await?;
            debug!("Album '{}' response: {}", self.name, serde_json::to_string_pretty(&response).unwrap_or_default());

            let query: super::cloudkit::QueryResponse = serde_json::from_value(response)?;
            let records = query.records;

            debug!(
                "Album '{}': got {} records at offset {}",
                self.name,
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

            for master in master_records {
                if let Some(asset_rec) = asset_records.remove(&master.record_name) {
                    all_assets.push(PhotoAsset::from_records(master, asset_rec));
                }
                offset += 1;
            }

            tracing::info!("  fetched {} photos so far...", all_assets.len());

            if let Some(n) = limit {
                if all_assets.len() >= n as usize {
                    all_assets.truncate(n as usize);
                    break;
                }
            }
        }

        Ok(all_assets)
    }

    fn list_query(&self, offset: u64, direction: &str) -> Value {
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

        if let Some(qf) = &self.query_filter {
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

        // resultsLimit is 2x page_size because each photo returns both a
        // CPLMaster and CPLAsset record — we need both to build a PhotoAsset.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct StubSession;

    #[async_trait::async_trait]
    impl PhotosSession for StubSession {
        async fn post(&self, _url: &str, _body: &str, _headers: &[(&str, &str)]) -> anyhow::Result<Value> {
            unimplemented!("stub")
        }
        async fn get(&self, _url: &str, _headers: &[(&str, &str)]) -> anyhow::Result<reqwest::Response> {
            unimplemented!("stub")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(StubSession)
        }
    }

    fn make_album(page_size: usize, query_filter: Option<Value>, zone_id: Value) -> PhotoAlbum {
        PhotoAlbum::new(
            HashMap::new(),
            Box::new(StubSession),
            "https://example.com".into(),
            "TestAlbum".into(),
            "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter,
            page_size,
            zone_id,
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
