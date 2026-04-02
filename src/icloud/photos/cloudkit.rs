use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Deserializes a JSON string, treating `null` as an empty string.
/// Used for fields like `recordType` that Apple sends as `null` for hard-deleted records.
fn string_or_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(std::option::Option::unwrap_or_default)
}

/// Response from `/zones/list`.
#[derive(Debug, Deserialize)]
pub(crate) struct ZoneListResponse {
    #[serde(default)]
    pub zones: Vec<Zone>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Zone {
    #[serde(rename = "zoneID")]
    pub zone_id: ZoneId,
    #[serde(default)]
    pub deleted: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ZoneId {
    pub zone_name: String,
    #[serde(flatten)]
    pub extra: Value,
}

/// Response from `/records/query`.
#[derive(Debug, Deserialize)]
pub(crate) struct QueryResponse {
    #[serde(default)]
    pub records: Vec<Record>,
    #[serde(default, rename = "syncToken")]
    pub sync_token: Option<String>,
}

/// Response from `/internal/records/query/batch`.
#[derive(Debug, Deserialize)]
pub(crate) struct BatchQueryResponse {
    #[serde(default)]
    pub batch: Vec<QueryResponse>,
}

/// A `CloudKit` record. Fields are kept as dynamic JSON because Apple's schema
/// varies by record type and changes without notice.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Record {
    #[serde(default)]
    pub record_name: String,
    #[serde(default, deserialize_with = "string_or_null")]
    pub record_type: String,
    #[serde(default)]
    pub fields: Value,
    #[serde(default)]
    pub deleted: Option<bool>,
}

/// Response from `/changes/database`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangesDatabaseResponse {
    pub sync_token: String,
    pub more_coming: bool,
    #[serde(default)]
    pub zones: Vec<ChangedZoneInfo>,
}

/// A zone that has changes, returned by `/changes/database`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangedZoneInfo {
    #[serde(rename = "zoneID")]
    pub zone_id: ZoneId,
    pub sync_token: String,
}

/// Response from `/changes/zone`.
#[derive(Debug, Deserialize)]
pub(crate) struct ChangesZoneResponse {
    pub zones: Vec<ChangesZoneResult>,
}

/// Result for a single zone in a `/changes/zone` response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangesZoneResult {
    #[serde(rename = "zoneID")]
    pub zone_id: ZoneId,
    pub sync_token: String,
    pub more_coming: bool,
    #[serde(default)]
    pub records: Vec<Record>,
    #[serde(default)]
    pub server_error_code: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
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

    #[test]
    fn test_query_response_with_sync_token_and_continuation() {
        let json = r#"{
            "records": [
                {"recordName": "R1", "recordType": "CPLAsset", "fields": {}}
            ],
            "syncToken": "st-abc-123",
            "continuationMarker": "cm-xyz-456"
        }"#;
        let resp: QueryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.records.len(), 1);
        assert_eq!(resp.sync_token.as_deref(), Some("st-abc-123"));
    }

    #[test]
    fn test_query_response_without_sync_token() {
        let json = r#"{"records": []}"#;
        let resp: QueryResponse = serde_json::from_str(json).unwrap();
        assert!(resp.sync_token.is_none());
    }

    #[test]
    fn test_record_with_change_tag_and_deleted() {
        let json = r#"{
            "recordName": "ABC",
            "recordType": "CPLAsset",
            "fields": {},
            "recordChangeTag": "ct-999",
            "deleted": false
        }"#;
        let rec: Record = serde_json::from_str(json).unwrap();
        assert_eq!(rec.deleted, Some(false));
    }

    #[test]
    fn test_record_without_change_tag_and_deleted() {
        let json = r#"{"recordName": "X", "recordType": "Y", "fields": {}}"#;
        let rec: Record = serde_json::from_str(json).unwrap();
        assert!(rec.deleted.is_none());
    }

    #[test]
    fn test_hard_deleted_record_null_record_type() {
        let json = r#"{
            "recordName": "ABC",
            "recordType": null,
            "deleted": true,
            "recordChangeTag": "tag"
        }"#;
        let rec: Record = serde_json::from_str(json).unwrap();
        assert_eq!(rec.record_name, "ABC");
        // null recordType deserializes as empty string via string_or_null
        assert_eq!(rec.record_type, "");
        assert_eq!(rec.deleted, Some(true));
    }

    #[test]
    fn test_changes_database_response_with_zones() {
        let json = r#"{
            "syncToken": "db-token-1",
            "moreComing": false,
            "zones": [
                {
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-token-1"
                },
                {
                    "zoneID": {"zoneName": "SharedSync-5678"},
                    "syncToken": "zone-token-2"
                }
            ]
        }"#;
        let resp: ChangesDatabaseResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.sync_token, "db-token-1");
        assert!(!resp.more_coming);
        assert_eq!(resp.zones.len(), 2);
        assert_eq!(resp.zones[0].zone_id.zone_name, "PrimarySync");
        assert_eq!(resp.zones[0].sync_token, "zone-token-1");
        assert_eq!(resp.zones[1].zone_id.zone_name, "SharedSync-5678");
    }

    #[test]
    fn test_changes_database_response_no_changes() {
        let json = r#"{
            "syncToken": "db-token-2",
            "moreComing": false,
            "zones": []
        }"#;
        let resp: ChangesDatabaseResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.sync_token, "db-token-2");
        assert!(!resp.more_coming);
        assert!(resp.zones.is_empty());
    }

    #[test]
    fn test_changed_zone_info() {
        let json = r#"{
            "zoneID": {"zoneName": "PrimarySync"},
            "syncToken": "zt-abc"
        }"#;
        let info: ChangedZoneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.zone_id.zone_name, "PrimarySync");
        assert_eq!(info.sync_token, "zt-abc");
    }

    #[test]
    fn test_changes_zone_response_with_records() {
        let json = r#"{
            "zones": [
                {
                    "zoneID": {"zoneName": "PrimarySync"},
                    "syncToken": "zt-new",
                    "moreComing": false,
                    "records": [
                        {"recordName": "R1", "recordType": "CPLAsset", "fields": {}, "recordChangeTag": "ct1"},
                        {"recordName": "R2", "recordType": "CPLMaster", "fields": {}, "recordChangeTag": "ct2"}
                    ]
                }
            ]
        }"#;
        let resp: ChangesZoneResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.zones.len(), 1);
        let zone = &resp.zones[0];
        assert_eq!(zone.zone_id.zone_name, "PrimarySync");
        assert_eq!(zone.sync_token, "zt-new");
        assert!(!zone.more_coming);
        assert_eq!(zone.records.len(), 2);
        assert_eq!(zone.records[0].record_name, "R1");
        assert!(zone.server_error_code.is_none());
        assert!(zone.reason.is_none());
    }

    #[test]
    fn test_changes_zone_result_with_error() {
        let json = r#"{
            "zoneID": {"zoneName": "PrimarySync"},
            "syncToken": "",
            "moreComing": false,
            "serverErrorCode": "ZONE_NOT_FOUND",
            "reason": "Zone does not exist"
        }"#;
        let result: ChangesZoneResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.server_error_code.as_deref(), Some("ZONE_NOT_FOUND"));
        assert_eq!(result.reason.as_deref(), Some("Zone does not exist"));
        assert!(result.records.is_empty());
    }

    #[test]
    fn test_changes_zone_result_more_coming() {
        let json = r#"{
            "zoneID": {"zoneName": "PrimarySync"},
            "syncToken": "partial-token",
            "moreComing": true,
            "records": []
        }"#;
        let result: ChangesZoneResult = serde_json::from_str(json).unwrap();
        assert!(result.more_coming);
        assert_eq!(result.sync_token, "partial-token");
        assert!(result.records.is_empty());
    }
}
