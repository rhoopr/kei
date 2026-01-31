use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Response from `/zones/list`.
#[derive(Debug, Deserialize)]
pub struct ZoneListResponse {
    #[serde(default)]
    pub zones: Vec<Zone>,
}

#[derive(Debug, Deserialize)]
pub struct Zone {
    #[serde(rename = "zoneID")]
    pub zone_id: ZoneId,
    #[serde(default)]
    pub deleted: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneId {
    pub zone_name: String,
    #[serde(flatten)]
    pub extra: Value,
}

/// Response from `/records/query`.
#[derive(Debug, Deserialize)]
pub struct QueryResponse {
    #[serde(default)]
    pub records: Vec<Record>,
}

/// Response from `/internal/records/query/batch`.
#[derive(Debug, Deserialize)]
pub struct BatchQueryResponse {
    #[serde(default)]
    pub batch: Vec<QueryResponse>,
}

/// A CloudKit record. Fields are kept as dynamic JSON because Apple's schema
/// varies by record type and changes without notice.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(default)]
    pub record_name: String,
    #[serde(default)]
    pub record_type: String,
    #[serde(default)]
    pub fields: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zone_list_response() {
        let json = r#"{
            "zones": [
                {
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "deleted": false
                },
                {
                    "zoneID": {"zoneName": "SharedSync-1234"},
                    "deleted": true
                }
            ]
        }"#;
        let resp: ZoneListResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.zones.len(), 2);
        assert_eq!(resp.zones[0].zone_id.zone_name, "PrimarySync");
        assert_eq!(resp.zones[1].deleted, Some(true));
    }

    #[test]
    fn test_query_response() {
        let json = r#"{
            "records": [
                {
                    "recordName": "ABC",
                    "recordType": "CPLAsset",
                    "fields": {"foo": {"value": "bar"}}
                }
            ]
        }"#;
        let resp: QueryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.records.len(), 1);
        assert_eq!(resp.records[0].record_name, "ABC");
        assert_eq!(resp.records[0].record_type, "CPLAsset");
    }

    #[test]
    fn test_batch_query_response() {
        let json = r#"{
            "batch": [
                {"records": [{"recordName": "X", "recordType": "Y", "fields": {}}]}
            ]
        }"#;
        let resp: BatchQueryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.batch.len(), 1);
        assert_eq!(resp.batch[0].records[0].record_name, "X");
    }

    #[test]
    fn test_query_response_empty() {
        let json = r#"{}"#;
        let resp: QueryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.records.is_empty());
    }

    #[test]
    fn test_record_missing_fields() {
        let json = r#"{"recordName": "A", "recordType": "B"}"#;
        let rec: Record = serde_json::from_str(json).unwrap();
        assert_eq!(rec.record_name, "A");
        assert!(rec.fields.is_null());
    }

    #[test]
    fn test_zone_id_round_trip() {
        let json = r#"{"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner", "zoneType": "REGULAR_CUSTOM_ZONE"}"#;
        let zone_id: ZoneId = serde_json::from_str(json).unwrap();
        assert_eq!(zone_id.zone_name, "PrimarySync");

        // Round-trip back to Value
        let value = serde_json::to_value(&zone_id).unwrap();
        assert_eq!(value["zoneName"], "PrimarySync");
        assert_eq!(value["ownerRecordName"], "_defaultOwner");
        assert_eq!(value["zoneType"], "REGULAR_CUSTOM_ZONE");

        // Ensure no duplicate zoneName from flatten
        let serialized = serde_json::to_string(&zone_id).unwrap();
        assert_eq!(serialized.matches("zoneName").count(), 1);
    }

    #[test]
    fn test_zone_list_empty() {
        let json = r#"{"zones": []}"#;
        let resp: ZoneListResponse = serde_json::from_str(json).unwrap();
        assert!(resp.zones.is_empty());
    }
}
