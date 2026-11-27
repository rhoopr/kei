//! Zone change feed for the sync core.
//!
//! `changes/zone` without a sync position enumerates the whole zone, so full
//! and incremental listings share this one path. Each page is parsed into
//! typed observations here, at the provider boundary. Download URLs are never
//! part of an observation: they expire, and the download step refreshes them
//! with [`lookup_records`].

use std::collections::HashSet;
use std::sync::LazyLock;

use anyhow::Context;
use serde_json::{Map, Value, json};

use super::PhotoLibrary;
use super::asset::{PhotoAsset, SparseShareEvidence, classify_change_reason};
use super::cloudkit::{ChangesZoneResponse, Record};
use super::queries::{
    DESIRED_KEYS, DESIRED_KEYS_VALUES, IDENTITY_LOOKUP_KEYS_VALUES, build_changes_zone_request,
};
use super::session::{check_changes_zone_error, retry_post, retry_post_allowing_record_errors};
use super::types::ChangeReason;

/// CloudKit serves at most 200 records per `changes/zone` page.
const PAGE_LIMIT: u32 = 200;
/// Records per `records/lookup` request.
pub(crate) const LOOKUP_BATCH: usize = 100;

const ROOT_FOLDER: &str = "----Root-Folder----";
const PROJECT_ROOT_FOLDER: &str = "----Project-Root-Folder----";
/// `albumType` for a folder that holds other albums.
const FOLDER_ALBUM_TYPE: i64 = 3;

static DESIRED_KEY_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| DESIRED_KEYS.iter().copied().collect());

/// One validated `changes/zone` page.
#[derive(Debug)]
pub(crate) struct FeedPage {
    pub(crate) observations: Vec<Observation>,
    /// Position after this page. Store it with the page's observations.
    pub(crate) sync_token: String,
    pub(crate) more_coming: bool,
}

/// A record from the feed, reduced to what the catalog stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observation {
    /// A `CPLAsset`. `body` is a URL-free stored record.
    Asset {
        name: String,
        master: Option<String>,
        body: String,
        deleted: bool,
        hidden: bool,
        /// A personal-library stand-in for a photo moved to the Shared
        /// Library. It has no media of its own here.
        sparse: Option<SparseLink>,
    },
    /// A `CPLMaster`. `body` is a URL-free stored record.
    Master { name: String, body: String },
    Album {
        name: String,
        parent: Option<String>,
        title: Option<String>,
        is_folder: bool,
        deleted: bool,
    },
    Membership {
        relation: String,
        album: String,
        asset: String,
        present: bool,
    },
    /// A purged record whose type iCloud no longer reports.
    Purged { name: String },
}

/// Where a sparse stand-in's photo lives, when its link is usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SparseLink {
    pub(crate) target: Option<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FeedError {
    /// iCloud no longer accepts the saved sync position. A full listing
    /// from no position can establish a new one.
    #[error("iCloud rejected the saved sync position: {0}")]
    PositionRejected(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl PhotoLibrary {
    /// Read one page of the zone change feed from `position`, or from the
    /// start of the zone when `position` is `None`.
    pub(crate) async fn feed_page(&self, position: Option<&str>) -> Result<FeedPage, FeedError> {
        let mut request = build_changes_zone_request(self.zone_id(), position, PAGE_LIMIT);
        if let Some(zone) = request
            .get_mut("zones")
            .and_then(|zones| zones.get_mut(0))
            .and_then(Value::as_object_mut)
        {
            // Include the sparse-share fields so stand-ins are recognized.
            zone.insert(
                "desiredKeys".into(),
                Value::Array(IDENTITY_LOOKUP_KEYS_VALUES.clone()),
            );
        }
        let response = retry_post(
            self.session(),
            &self.endpoint_url("changes/zone"),
            &request.to_string(),
            &[("Content-type", "text/plain")],
            self.retry_config(),
        )
        .await?;
        let mut response: ChangesZoneResponse =
            serde_json::from_value(response).context("iCloud returned a malformed changes page")?;
        if response.zones.len() != 1 {
            return Err(anyhow::anyhow!(
                "iCloud returned {} zones for a single-zone changes request",
                response.zones.len()
            )
            .into());
        }
        let zone = response.zones.remove(0);
        if let Err(error) = check_changes_zone_error(
            zone.server_error_code.as_deref(),
            zone.reason.as_deref(),
            self.zone_name(),
        ) {
            if error.should_fallback_to_full() && position.is_some() {
                return Err(FeedError::PositionRejected(error.to_string()));
            }
            return Err(FeedError::Other(error.into()));
        }
        if zone.sync_token.trim().is_empty() {
            return Err(
                anyhow::anyhow!("iCloud returned a changes page without a sync position").into(),
            );
        }
        let observations = zone
            .records
            .into_iter()
            .map(observe)
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(FeedPage {
            observations,
            sync_token: zone.sync_token,
            more_coming: zone.more_coming,
        })
    }

    /// `CPLAsset` record names in a queried smart folder, from iCloud's index.
    pub(crate) async fn smart_folder_members(&self, name: &str) -> anyhow::Result<HashSet<String>> {
        const PAGE: u64 = 200;
        const MAX_PAGES: usize = 100_000;
        let folders = super::smart_folders::smart_folders();
        let (_, folder) = folders
            .iter()
            .find(|(known, _)| *known == name)
            .with_context(|| format!("Unknown smart folder {name}"))?;
        let mut members = HashSet::new();
        let mut offset = 0u64;
        for _ in 0..MAX_PAGES {
            let mut filter = vec![
                json!({"fieldName": "startRank", "fieldValue": {"type": "INT64", "value": offset}, "comparator": "EQUALS"}),
                json!({"fieldName": "direction", "fieldValue": {"type": "STRING", "value": "ASCENDING"}, "comparator": "EQUALS"}),
            ];
            if let Some(extra) = folder.query_filter.as_deref().and_then(Value::as_array) {
                filter.extend(extra.iter().cloned());
            }
            let body = json!({
                "query": {"filterBy": filter, "recordType": folder.list_type},
                "resultsLimit": PAGE * 2,
                "desiredKeys": ["recordName", "recordType"],
                "zoneID": self.zone_id(),
            });
            let response = retry_post(
                self.session(),
                &self.endpoint_url("records/query"),
                &body.to_string(),
                &[("Content-type", "text/plain")],
                self.retry_config(),
            )
            .await?;
            let records: Vec<Record> = serde_json::from_value(
                response
                    .get("records")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            )
            .context("iCloud returned a malformed smart folder page")?;
            let before = members.len();
            let assets = records
                .into_iter()
                .filter(|record| record.record_type == "CPLAsset" && !record.record_name.is_empty())
                .map(|record| members.insert(record.record_name))
                .count();
            if assets == 0 {
                return Ok(members);
            }
            anyhow::ensure!(
                members.len() > before,
                "iCloud repeated a smart folder page"
            );
            offset += u64::try_from(assets)?;
        }
        anyhow::bail!("Smart folder {name} has more pages than kei reads")
    }

    /// Fetch current records by name, with download URLs. Names iCloud no
    /// longer knows are omitted from the result.
    pub(crate) async fn lookup_records(&self, names: &[String]) -> anyhow::Result<Vec<Record>> {
        anyhow::ensure!(names.len() <= LOOKUP_BATCH, "Lookup batch is too large");
        let body = json!({
            "records": names.iter().map(|name| json!({"recordName": name})).collect::<Vec<_>>(),
            "zoneID": self.zone_id(),
            "desiredKeys": &*DESIRED_KEYS_VALUES,
        });
        let response = retry_post_allowing_record_errors(
            self.session(),
            &self.endpoint_url("records/lookup"),
            &body.to_string(),
            &[("Content-type", "text/plain")],
            self.retry_config(),
        )
        .await?;
        let records = response
            .get("records")
            .and_then(Value::as_array)
            .context("iCloud returned a malformed lookup response")?;
        let mut found = Vec::with_capacity(records.len());
        for record in records {
            if record.get("serverErrorCode").is_some() {
                continue;
            }
            let record: Record = serde_json::from_value(record.clone())
                .context("iCloud returned a malformed lookup record")?;
            if record.deleted != Some(true) && !record.record_name.is_empty() {
                found.push(record);
            }
        }
        Ok(found)
    }
}

/// How the core knows a smart folder's members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SmartRule {
    Favorites,
    Hidden,
    RecentlyDeleted,
    Videos,
    Live,
    /// Membership comes from iCloud's own index for the folder.
    Queried,
}

/// The rule for a smart folder kei recognizes, by its display name.
pub(crate) fn smart_rule(name: &str) -> Option<SmartRule> {
    let known = super::smart_folders::smart_folders()
        .iter()
        .any(|(known, _)| *known == name);
    let rule = match name {
        "Favorites" => SmartRule::Favorites,
        "Hidden" => SmartRule::Hidden,
        "Recently Deleted" => SmartRule::RecentlyDeleted,
        "Videos" => SmartRule::Videos,
        "Live" => SmartRule::Live,
        _ => SmartRule::Queried,
    };
    known.then_some(rule)
}

/// Whether a derived smart folder contains a photo. Queried folders answer
/// `false` here; their members come from [`PhotoLibrary::smart_folder_members`].
pub(crate) fn in_derived_smart_folder(
    rule: SmartRule,
    photo: &PhotoAsset,
    hidden: bool,
    deleted: bool,
) -> bool {
    match rule {
        SmartRule::Hidden => hidden && !deleted,
        SmartRule::RecentlyDeleted => deleted,
        _ if hidden || deleted => false,
        SmartRule::Favorites => photo.metadata().is_favorite,
        SmartRule::Videos => photo.item_type() == Some(super::types::AssetItemType::Movie),
        SmartRule::Live => photo.is_live_photo(),
        SmartRule::Queried => false,
    }
}

/// Build an asset from two stored catalog bodies. Its versions have no URLs.
pub(crate) fn stored_asset(master: &str, asset: &str) -> anyhow::Result<PhotoAsset> {
    let master: Record = serde_json::from_str(master).context("Corrupt stored master record")?;
    let asset: Record = serde_json::from_str(asset).context("Corrupt stored asset record")?;
    Ok(PhotoAsset::from_stored_records(master, &asset))
}

/// Build an asset from freshly looked-up records, with download URLs.
pub(crate) fn current_asset(master: Record, asset: &Record) -> PhotoAsset {
    PhotoAsset::from_records(master, asset)
}

fn observe(record: Record) -> anyhow::Result<Option<Observation>> {
    anyhow::ensure!(
        !record.record_name.trim().is_empty(),
        "iCloud returned a record without a name"
    );
    let reason = classify_change_reason(&record);
    if reason == ChangeReason::HardDeleted {
        return Ok(Some(match relation_parts(&record.record_name) {
            Some((asset, album))
                if record.record_type.is_empty()
                    || record.record_type == "CPLContainerRelation" =>
            {
                Observation::Membership {
                    relation: record.record_name.clone(),
                    album: album.to_owned(),
                    asset: asset.to_owned(),
                    present: false,
                }
            }
            _ if record.record_type == "CPLAlbum" => Observation::Album {
                name: record.record_name,
                parent: None,
                title: None,
                is_folder: false,
                deleted: true,
            },
            _ => Observation::Purged {
                name: record.record_name,
            },
        }));
    }
    Ok(match record.record_type.as_str() {
        "CPLAsset" => Some(Observation::Asset {
            master: reference(&record.fields, "masterRef"),
            deleted: reason == ChangeReason::SoftDeleted,
            hidden: flag(&record.fields, "isHidden"),
            sparse: SparseShareEvidence::from_fields(&record.fields).map(|evidence| SparseLink {
                target: match evidence {
                    SparseShareEvidence::Linked(link) => {
                        let (zone, record) = link.target();
                        Some((zone.to_owned(), record.to_owned()))
                    }
                    SparseShareEvidence::Malformed => None,
                },
            }),
            body: stored_body(&record)?,
            name: record.record_name,
        }),
        "CPLMaster" => Some(Observation::Master {
            body: stored_body(&record)?,
            name: record.record_name,
        }),
        "CPLAlbum" => {
            if record.record_name == ROOT_FOLDER || record.record_name == PROJECT_ROOT_FOLDER {
                return Ok(None);
            }
            Some(Observation::Album {
                parent: string_value(&record.fields, "parentId")
                    .filter(|parent| parent != ROOT_FOLDER && parent != PROJECT_ROOT_FOLDER),
                title: super::enc::album_name(&record.fields),
                is_folder: record
                    .fields
                    .pointer("/albumType/value")
                    .and_then(Value::as_i64)
                    == Some(FOLDER_ALBUM_TYPE),
                deleted: reason == ChangeReason::SoftDeleted,
                name: record.record_name,
            })
        }
        "CPLContainerRelation" => {
            let album = string_value(&record.fields, "containerId");
            let asset = string_value(&record.fields, "itemId");
            let (Some(album), Some(asset)) = (album, asset) else {
                anyhow::bail!("iCloud returned an album membership without its album or photo");
            };
            Some(Observation::Membership {
                relation: record.record_name,
                album,
                asset,
                present: reason != ChangeReason::SoftDeleted,
            })
        }
        _ => None,
    })
}

/// Relation record names have the form `<asset>-IN-<album>`.
fn relation_parts(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once("-IN-")
        .filter(|(asset, album)| !asset.is_empty() && !album.is_empty())
}

fn string_value(fields: &Value, key: &str) -> Option<String> {
    fields
        .get(key)?
        .get("value")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn reference(fields: &Value, key: &str) -> Option<String> {
    let value = fields.get(key)?.get("value")?;
    value
        .get("recordName")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn flag(fields: &Value, key: &str) -> bool {
    super::metadata::change_flag_is_set(fields, key)
}

/// Keep the fields the catalog needs and drop download URLs and wrapping
/// keys from every resource. The result is what the catalog stores.
fn stored_body(record: &Record) -> anyhow::Result<String> {
    let mut fields = Map::new();
    if let Some(source) = record.fields.as_object() {
        for (key, value) in source {
            if !DESIRED_KEY_SET.contains(key.as_str()) {
                continue;
            }
            let mut value = value.clone();
            if key.ends_with("Res")
                && let Some(resource) = value.get_mut("value").and_then(Value::as_object_mut)
            {
                resource.remove("downloadURL");
                resource.remove("wrappingKey");
            }
            fields.insert(key.clone(), value);
        }
    }
    Ok(serde_json::to_string(&json!({
        "recordName": record.record_name,
        "recordType": record.record_type,
        "fields": fields,
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(value: Value) -> Record {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn stored_bodies_drop_urls_and_unknown_fields() {
        let master = record(json!({
            "recordName": "M1",
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": "IMG_0001.JPG", "type": "STRING"},
                "resOriginalRes": {"value": {
                    "size": 3, "fileChecksum": "AQID",
                    "downloadURL": "https://cvws.icloud-content.com/x", "wrappingKey": "k"
                }},
                "unrelated": {"value": 1}
            }
        }));
        let body = stored_body(&master).unwrap();
        assert!(!body.contains("downloadURL"));
        assert!(!body.contains("wrappingKey"));
        assert!(!body.contains("unrelated"));
        assert!(body.contains("fileChecksum"));
    }

    #[test]
    fn observations_classify_feed_records() {
        let asset = observe(record(json!({
            "recordName": "A1", "recordType": "CPLAsset",
            "fields": {"masterRef": {"value": {"recordName": "M1"}}, "isHidden": {"value": 1}}
        })))
        .unwrap()
        .unwrap();
        assert!(matches!(
            asset,
            Observation::Asset { ref master, hidden: true, deleted: false, .. } if master.as_deref() == Some("M1")
        ));
        let purged = observe(record(json!({"recordName": "X", "deleted": true}))).unwrap();
        assert_eq!(purged, Some(Observation::Purged { name: "X".into() }));
        let removed = observe(record(json!({"recordName": "A1-IN-AL1", "deleted": true}))).unwrap();
        assert_eq!(
            removed,
            Some(Observation::Membership {
                relation: "A1-IN-AL1".into(),
                album: "AL1".into(),
                asset: "A1".into(),
                present: false
            })
        );
        let root = observe(record(
            json!({"recordName": ROOT_FOLDER, "recordType": "CPLAlbum", "fields": {}}),
        ));
        assert_eq!(root.unwrap(), None);
        assert!(
            observe(record(
                json!({"recordName": "R", "recordType": "CPLContainerRelation", "fields": {}})
            ))
            .is_err()
        );
    }

    #[test]
    fn sparse_stand_ins_keep_their_link() {
        let linked = observe(record(json!({
            "recordName": "P1", "recordType": "CPLAsset",
            "fields": {
                "isSparsePrivateRecord": {"value": 1, "type": "INT64"},
                "linkedShareRecordName": {"value": "S1", "type": "STRING"},
                "linkedShareZoneName": {"value": "SharedSync-ABC", "type": "STRING"},
                "linkedShareZoneOwner": {"value": {"recordName": "_owner"}, "type": "REFERENCE"}
            }
        })))
        .unwrap()
        .unwrap();
        let Observation::Asset {
            sparse: Some(link), ..
        } = linked
        else {
            panic!("expected a sparse asset");
        };
        assert_eq!(link.target, Some(("SharedSync-ABC".into(), "S1".into())));
    }

    #[test]
    fn smart_rules_cover_every_known_folder() {
        for name in super::super::smart_folders::smart_folder_names() {
            assert!(smart_rule(name).is_some(), "{name}");
        }
        assert_eq!(smart_rule("Favorites"), Some(SmartRule::Favorites));
        assert_eq!(smart_rule("Screenshots"), Some(SmartRule::Queried));
        assert_eq!(smart_rule("Holiday"), None);
    }

    #[test]
    fn stored_assets_keep_versions_without_urls() {
        let master = record(json!({
            "recordName": "M1", "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": "IMG_0001.JPG", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {"size": 3, "fileChecksum": "AQID", "downloadURL": "https://cvws.icloud-content.com/x"}},
                "resOriginalFileType": {"value": "public.jpeg"}
            }
        }));
        let asset = record(json!({
            "recordName": "A1", "recordType": "CPLAsset",
            "fields": {"masterRef": {"value": {"recordName": "M1"}}, "assetDate": {"value": 1_700_000_000_000.0_f64}}
        }));
        let stored = stored_asset(
            &stored_body(&master).unwrap(),
            &stored_body(&asset).unwrap(),
        )
        .unwrap();
        let original = stored
            .get_version(crate::icloud::photos::types::AssetVersionSize::Original)
            .unwrap();
        assert_eq!(original.size, 3);
        assert_eq!(&*original.url, "kei-stored:AQID");
        assert_eq!(stored.filename(), Some("IMG_0001.JPG"));
    }
}
