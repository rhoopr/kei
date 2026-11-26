//! A new checkpoint needs current coverage, not a repaired historical receipt.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::PhotosSession;
use crate::state::SparseIdentityStore;
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    MutableCaptureSession, RUN_CYCLE_ASSET_DATE_MS, RunCycleDownloadConfigOptions,
    album_count_response, full_album_page_with_download, make_captured_cycle_album,
    make_owned_cycle_album, make_owned_cycle_album_without_request_owner,
    make_owned_cycle_container_album, make_owned_cycle_smart_folder, make_run_cycle_config,
    make_run_cycle_download_config_builder_with_options, make_run_cycle_library_state_with_album,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
    media_without_photo_downloads, open_owned_cycle_db,
};
use crate::{download, state};

const ZONE: &str = "PrimarySync";
const HEALTHY_ZONE: &str = "SharedSync-CLEAN";
const HEALTHY_MASTER: &str = "healthy-master";
const ACCEPTED_MASTER: &str = "accepted-page-master";
const ACCEPTED_ALBUM: &str = "Replay Album";
const FAVORITES: &str = "Favorites";
const FOREIGN_ZONE: &str = "SharedSync-MEDIA";
const ACCEPTED_CONTAINER: &str = "container-replay-album";
const ACCEPTED_RELATION: &str = "relation-accepted-page-photo";
const RETAINED_LINK_CURSOR: &str = "earlier-album-link-cursor";
const REMOVED_LINK: &str = "asset-accepted-page-master-IN-container-replay-album";
const SECOND_REMOVED_LINK: &str = "asset-accepted-page-master-IN-container-unresolved-album";
const UNRESOLVED_MASTER: &str = "unresolved-master";
const UNRESOLVED_ALBUM: &str = "Unresolved Album";
const UNRESOLVED_CONTAINER: &str = "container-unresolved-album";
const UNRESOLVED_RELATION: &str = "relation-unresolved-photo";
const CURSOR_KEY: &str = "sync_token:PrimarySync";
const HOLD_KEY: &str = "retained_checkpoint_hold:PrimarySync";
const PRIOR: &str = "private-retained-cursor";
const CONTINUATION: &str = "accepted-page";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    Rebaseline,
    Eligibility,
    Sparse,
    Legacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    MissingSuccessor,
    MissingContinuation,
    MissingContinuationFlag,
    NonBooleanContinuationFlag,
    InvalidReasonOnContinuation,
    RejectedContinuation,
    RejectedContinuationAfterMedia(AcceptedMedia),
    WrongZoneOnContinuation,
    MissingZoneNameOnContinuation,
    MissingZoneIdOnContinuation,
    EmptyZonesOnContinuation,
    MultipleZonesOnContinuation,
    NullZonesOnContinuation,
    NullZoneEntryOnContinuation,
    NullResponseOnContinuation,
    MalformedJsonOnContinuation,
    MalformedResponseError,
    RedirectOnContinuation,
    DuplicateJsonKeysOnContinuation,
    OversizedBodyOnContinuation,
    WrongOwnerOnContinuation,
    MalformedOwnerOnContinuation,
    MalformedOwnerOnOwnerlessContinuation,
    RepeatedContinuation,
    CyclicContinuation,
    RecordErrorOnContinuation,
    ForeignRecordZoneOnContinuation,
    BlankRecordNameOnContinuation,
    MissingRecordsOnContinuation,
    NonArrayRecordsOnContinuation,
    NonObjectRecordOnContinuation,
    ZoneNotFoundOnContinuation,
    ZoneNotFoundOnFirstReplay,
    RetryLaterOnContinuation,
    RetryLaterOnFirstReplay,
    ServerErrorOnContinuation,
    ServerErrorOnFirstReplay,
    UnrecognizedZoneErrorOnContinuation,
    ThrottledOnContinuation,
    ThrottledOnFirstReplay,
    AuthContinuation,
    Partial,
    Cancel,
    HoldWrite,
    StalePlan,
    ConcurrentSparse,
    ConcurrentCursor,
    ConcurrentConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcceptedMedia {
    Confirmed,
    ChangedOnLookup,
    CurrentAlbumMember,
    RemovedAlbumMember,
    MovedAlbumMember,
    MixedAlbums,
    OverlappingAlbums,
    OverlapWithRemovedLink,
    ConfirmedUnfiled,
    InconclusiveUnfiled,
    TwoAlbumRemovals {
        missing: Option<&'static str>,
    },
    MissingMaster(MissingMasterLookup),
    RetainedLink(RetainedLinkLookup),
    Favorites(FavoriteLookup),
    FavoritesUnfiled {
        unfiled_first: bool,
        favorite: FavoriteLookup,
        same_folder: bool,
    },
    ForeignAlbum(ForeignLookup),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MissingMasterLookup {
    Confirmed,
    Omitted,
    ChangedChild,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetainedLinkLookup {
    Current,
    Omitted,
    WrongAlbum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FavoriteLookup {
    Current,
    Missing,
    Unmarked,
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForeignLookup {
    Current,
    MissingLink,
    UnknownOwnerItem,
    OwnerNamesake,
    MissingMaster,
    WrongMediaScope,
}

#[derive(Clone)]
struct ForeignMediaSession {
    records: Arc<Vec<Value>>,
    result: ForeignLookup,
    lookups: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for ForeignMediaSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        assert!(
            url.contains("/records/lookup?"),
            "foreign replay must not scan"
        );
        let request: Value = serde_json::from_str(&body)?;
        assert_eq!(
            request["zoneID"],
            json!({"zoneName": FOREIGN_ZONE, "ownerRecordName": "_defaultOwner"})
        );
        let child = format!("asset-{ACCEPTED_MASTER}");
        let first = self
            .lookups
            .fetch_add(1, Ordering::SeqCst)
            .is_multiple_of(2);
        assert_eq!(request["records"][0]["recordName"], child);
        assert_eq!(
            request["records"].as_array().unwrap().len(),
            if first { 1 } else { 2 }
        );
        let mut records = vec![self.records[1].clone()];
        records[0]["fields"]["masterRef"]["value"]["zoneID"]["zoneName"] = json!(FOREIGN_ZONE);
        if !first {
            assert_eq!(request["records"][1]["recordName"], ACCEPTED_MASTER);
            if self.result != ForeignLookup::MissingMaster {
                records.push(self.records[0].clone());
            }
            if self.result == ForeignLookup::WrongMediaScope {
                records[1]["zoneID"] =
                    json!({"zoneName": ZONE, "ownerRecordName": "_defaultOwner"});
            }
        }
        Ok(json!({"records": records}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

impl AcceptedMedia {
    fn overlaps(self) -> bool {
        matches!(self, Self::OverlappingAlbums | Self::OverlapWithRemovedLink)
    }

    fn tests_unfiled_removal(self) -> bool {
        matches!(
            self,
            Self::ConfirmedUnfiled | Self::InconclusiveUnfiled | Self::TwoAlbumRemovals { .. }
        )
    }
}

impl Fault {
    fn first_replay_zone_error(self) -> Option<&'static str> {
        match self {
            Self::ZoneNotFoundOnFirstReplay => Some("ZONE_NOT_FOUND"),
            Self::RetryLaterOnFirstReplay => Some("RETRY_LATER"),
            Self::ServerErrorOnFirstReplay => Some("SERVER_INTERNAL_ERROR"),
            Self::ThrottledOnFirstReplay => Some("THROTTLED"),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct RetainedSession {
    records: Arc<Vec<Value>>,
    reject_prior: bool,
    fault: Fault,
    database: PathBuf,
    cancel: CancellationToken,
    queries: Arc<AtomicUsize>,
    rank_starts: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    lookups: Arc<AtomicUsize>,
    membership_lookups: Arc<AtomicUsize>,
    inventories: Arc<AtomicUsize>,
    retained: Arc<AtomicUsize>,
    injected: Arc<AtomicBool>,
    http_continuation: Option<(reqwest::Client, String)>,
}

#[async_trait::async_trait]
impl PhotosSession for RetainedSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response(
                self.records
                    .iter()
                    .filter(|record| record["recordType"] == "CPLAsset")
                    .count() as u64,
            ));
        }
        if url.contains("/records/lookup?") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let names = request["records"].as_array().unwrap();
            if let Fault::RejectedContinuationAfterMedia(AcceptedMedia::ForeignAlbum(result)) =
                self.fault
            {
                assert_eq!(
                    request["zoneID"],
                    json!({"zoneName": ZONE, "ownerRecordName": "_defaultOwner"})
                );
                let child = format!("asset-{ACCEPTED_MASTER}");
                assert_eq!(
                    request["records"],
                    json!([{"recordName": child}, {"recordName": ACCEPTED_RELATION}])
                );
                self.membership_lookups.fetch_add(1, Ordering::SeqCst);
                let mut records = Vec::new();
                if result != ForeignLookup::MissingLink {
                    records.push(replay_album_link(
                        ACCEPTED_RELATION,
                        ACCEPTED_CONTAINER,
                        ACCEPTED_MASTER,
                    ));
                }
                match result {
                    ForeignLookup::UnknownOwnerItem => {}
                    ForeignLookup::OwnerNamesake => records.push(self.records[1].clone()),
                    _ => records.push(json!({"recordName":child,"serverErrorCode":"UNKNOWN_ITEM"})),
                }
                return Ok(json!({"records": records}));
            }
            let second_master = match self.fault {
                Fault::RejectedContinuationAfterMedia(case) if case.overlaps() => ACCEPTED_MASTER,
                _ => UNRESOLVED_MASTER,
            };
            for (link, master) in [
                (ACCEPTED_RELATION, ACCEPTED_MASTER),
                (UNRESOLVED_RELATION, second_master),
                (REMOVED_LINK, ACCEPTED_MASTER),
                (SECOND_REMOVED_LINK, ACCEPTED_MASTER),
            ] {
                if let Some(relation) = names.iter().find(|record| record["recordName"] == link) {
                    self.membership_lookups.fetch_add(1, Ordering::SeqCst);
                    let zone = relation.get("zoneID").unwrap_or(&request["zoneID"]);
                    assert_eq!(zone["zoneName"], ZONE);
                    assert_eq!(zone["ownerRecordName"], "_defaultOwner");
                    if matches!(
                        self.fault,
                        Fault::RejectedContinuationAfterMedia(
                            AcceptedMedia::TwoAlbumRemovals { .. }
                        )
                    ) {
                        assert_eq!(names.len(), 4);
                        for removal in [REMOVED_LINK, SECOND_REMOVED_LINK] {
                            assert!(names.iter().any(|record| record["recordName"] == removal));
                        }
                    } else {
                        assert_eq!(names.len(), 3);
                    }
                    let child = format!("asset-{master}");
                    for name in [master, child.as_str()] {
                        assert!(names.iter().any(|record| record["recordName"] == name));
                    }
                }
            }
            let mut records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    names
                        .iter()
                        .any(|name| name["recordName"] == record["recordName"])
                })
                .cloned()
                .collect();
            if let Fault::RejectedContinuationAfterMedia(
                AcceptedMedia::Favorites(favorite)
                | AcceptedMedia::FavoritesUnfiled { favorite, .. },
            ) = self.fault
            {
                assert_eq!(request["zoneID"]["zoneName"], ZONE);
                assert_eq!(request["zoneID"]["ownerRecordName"], "_defaultOwner");
                assert_eq!(names.len(), 2);
                let child = format!("asset-{ACCEPTED_MASTER}");
                for name in [child.as_str(), ACCEPTED_MASTER] {
                    assert!(names.iter().any(|record| record["recordName"] == name));
                }
                assert!(
                    request["desiredKeys"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("isFavorite"))
                );
                let fields = records
                    .iter_mut()
                    .find(|record| record["recordName"] == child)
                    .unwrap()["fields"]
                    .as_object_mut()
                    .unwrap();
                match favorite {
                    FavoriteLookup::Current => {}
                    FavoriteLookup::Missing => {
                        fields.remove("isFavorite");
                    }
                    FavoriteLookup::Unmarked => {
                        fields.get_mut("isFavorite").unwrap()["value"] = json!(0);
                    }
                    FavoriteLookup::Malformed => {
                        fields.get_mut("isFavorite").unwrap()["value"] = json!(2);
                    }
                }
            }
            if let Fault::RejectedContinuationAfterMedia(AcceptedMedia::MissingMaster(result)) =
                self.fault
            {
                assert_eq!(request["zoneID"]["zoneName"], ZONE);
                assert_eq!(request["zoneID"]["ownerRecordName"], "_defaultOwner");
                assert_eq!(names.len(), 2);
                let child = format!("asset-{ACCEPTED_MASTER}");
                for name in [child.as_str(), ACCEPTED_MASTER] {
                    assert!(names.iter().any(|record| record["recordName"] == name));
                }
                match result {
                    MissingMasterLookup::Confirmed => {}
                    MissingMasterLookup::Omitted => {
                        records.retain(|record| record["recordName"] != ACCEPTED_MASTER);
                    }
                    MissingMasterLookup::ChangedChild => {
                        let record = records
                            .iter_mut()
                            .find(|record| record["recordName"] == child)
                            .unwrap();
                        record["fields"]["assetDate"]["value"] =
                            json!(RUN_CYCLE_ASSET_DATE_MS + 86_400_000);
                    }
                }
            }
            let missing = match self.fault {
                Fault::RejectedContinuationAfterMedia(AcceptedMedia::InconclusiveUnfiled) => {
                    Some(REMOVED_LINK)
                }
                Fault::RejectedContinuationAfterMedia(AcceptedMedia::TwoAlbumRemovals {
                    missing,
                }) => missing,
                Fault::RejectedContinuationAfterMedia(AcceptedMedia::RetainedLink(
                    RetainedLinkLookup::Omitted,
                )) => Some(ACCEPTED_RELATION),
                _ => None,
            };
            if let Some(missing) = missing {
                records.retain(|record| record["recordName"] != missing);
            }
            if self.fault == Fault::RejectedContinuationAfterMedia(AcceptedMedia::ChangedOnLookup)
                && let Some(master) = records
                    .iter_mut()
                    .find(|record| record["recordName"] == ACCEPTED_MASTER)
            {
                master["fields"]["resOriginalRes"]["value"]["size"] = json!(1);
            }
            if let Some(relation) = records
                .iter_mut()
                .find(|record| record["recordName"] == ACCEPTED_RELATION)
            {
                match self.fault {
                    Fault::RejectedContinuationAfterMedia(AcceptedMedia::RemovedAlbumMember) => {
                        relation["deleted"] = json!(true);
                    }
                    Fault::RejectedContinuationAfterMedia(AcceptedMedia::MovedAlbumMember) => {
                        relation["fields"]["containerId"]["value"] = json!("another-album");
                    }
                    Fault::RejectedContinuationAfterMedia(AcceptedMedia::RetainedLink(
                        RetainedLinkLookup::WrongAlbum,
                    )) => relation["fields"]["containerId"]["value"] = json!("another-album"),
                    _ => {}
                }
            }
            if self.fault != Fault::RejectedContinuationAfterMedia(AcceptedMedia::OverlappingAlbums)
                && let Some(relation) = records
                    .iter_mut()
                    .find(|record| record["recordName"] == UNRESOLVED_RELATION)
            {
                relation["deleted"] = json!(true);
            }
            return Ok(json!({"records": records}));
        }
        if url.contains("/records/query?") {
            self.queries.fetch_add(1, Ordering::SeqCst);
            if self.fault == Fault::ConcurrentSparse && !self.injected.swap(true, Ordering::SeqCst)
            {
                // A separate connection injects work after the cycle has planned
                // its inventory. The fixture, not selection code, owns this event.
                let db = open_owned_cycle_db(&self.database).await;
                seed_sparse(&db, "arrived-during-scan").await;
            }
            if self.fault == Fault::ConcurrentCursor && !self.injected.swap(true, Ordering::SeqCst)
            {
                rusqlite::Connection::open(&self.database)?.execute(
                    "UPDATE metadata SET value='independently-advanced-cursor' WHERE key='sync_token:PrimarySync'", [],
                )?;
            }
            if self.fault == Fault::ConcurrentConfig && !self.injected.swap(true, Ordering::SeqCst)
            {
                rusqlite::Connection::open(&self.database)?.execute(
                    "UPDATE metadata SET value='independently-activated-config' WHERE key='enum_config_hash'", [],
                )?;
            }
            if self.fault == Fault::Cancel {
                self.cancel.cancel();
            }
            if self.fault == Fault::Partial {
                return Ok(json!({"records": null, "syncToken": "fresh-rank-anchor"}));
            }
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.rank_starts.fetch_add(1, Ordering::SeqCst);
                self.records.as_ref().clone()
            } else {
                Vec::new()
            };
            return Ok(json!({"records": records, "syncToken": "fresh-rank-anchor"}));
        }
        assert!(
            url.contains("/changes/zone?"),
            "unexpected retained fixture request"
        );
        if self.fault == Fault::MalformedOwnerOnOwnerlessContinuation {
            assert!(
                request["zones"][0]["zoneID"]
                    .get("ownerRecordName")
                    .is_none()
            );
        }
        let token = request["zones"][0]["syncToken"].as_str();
        if token == Some(RETAINED_LINK_CURSOR) {
            return Ok(json!({"zones": [{
                "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                "syncToken": "earlier-album-link-successor", "moreComing": false,
                "records": [replay_album_link(ACCEPTED_RELATION, ACCEPTED_CONTAINER, ACCEPTED_MASTER)]
            }]}));
        }
        if token == Some(PRIOR) {
            self.retained.fetch_add(1, Ordering::SeqCst);
            if matches!(
                self.fault,
                Fault::MissingContinuation
                    | Fault::MissingContinuationFlag
                    | Fault::NonBooleanContinuationFlag
                    | Fault::InvalidReasonOnContinuation
                    | Fault::RejectedContinuation
                    | Fault::RejectedContinuationAfterMedia(_)
                    | Fault::WrongZoneOnContinuation
                    | Fault::MissingZoneNameOnContinuation
                    | Fault::MissingZoneIdOnContinuation
                    | Fault::EmptyZonesOnContinuation
                    | Fault::MultipleZonesOnContinuation
                    | Fault::NullZonesOnContinuation
                    | Fault::NullZoneEntryOnContinuation
                    | Fault::NullResponseOnContinuation
                    | Fault::MalformedJsonOnContinuation
                    | Fault::MalformedResponseError
                    | Fault::RedirectOnContinuation
                    | Fault::DuplicateJsonKeysOnContinuation
                    | Fault::OversizedBodyOnContinuation
                    | Fault::WrongOwnerOnContinuation
                    | Fault::MalformedOwnerOnContinuation
                    | Fault::MalformedOwnerOnOwnerlessContinuation
                    | Fault::RepeatedContinuation
                    | Fault::CyclicContinuation
                    | Fault::RecordErrorOnContinuation
                    | Fault::ForeignRecordZoneOnContinuation
                    | Fault::BlankRecordNameOnContinuation
                    | Fault::MissingRecordsOnContinuation
                    | Fault::NonArrayRecordsOnContinuation
                    | Fault::NonObjectRecordOnContinuation
                    | Fault::ZoneNotFoundOnContinuation
                    | Fault::RetryLaterOnContinuation
                    | Fault::ServerErrorOnContinuation
                    | Fault::UnrecognizedZoneErrorOnContinuation
                    | Fault::ThrottledOnContinuation
                    | Fault::AuthContinuation
            ) {
                let records = if matches!(self.fault, Fault::RejectedContinuationAfterMedia(_)) {
                    let missing_master = matches!(
                        self.fault,
                        Fault::RejectedContinuationAfterMedia(AcceptedMedia::MissingMaster(_))
                    );
                    json!(
                        self.records
                            .iter()
                            .filter(|record| {
                                if matches!(
                                    self.fault,
                                    Fault::RejectedContinuationAfterMedia(
                                        AcceptedMedia::ForeignAlbum(_)
                                    )
                                ) {
                                    return record["recordName"] == ACCEPTED_RELATION;
                                }
                                (!missing_master || record["recordName"] != ACCEPTED_MASTER)
                                    && (!matches!(
                                        self.fault,
                                        Fault::RejectedContinuationAfterMedia(
                                            AcceptedMedia::RetainedLink(_)
                                        )
                                    ) || record["recordName"] != ACCEPTED_RELATION)
                            })
                            .collect::<Vec<_>>()
                    )
                } else {
                    json!([{"recordName":"accepted-change", "recordType":"FutureType"}])
                };
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": CONTINUATION, "moreComing": true,
                    "records": records
                }]}));
            }
            let code = self.fault.first_replay_zone_error();
            if self.fault == Fault::MissingSuccessor || code.is_some() {
                let records = if self.fault == Fault::ZoneNotFoundOnFirstReplay {
                    json!([{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}])
                } else {
                    json!([])
                };
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "serverErrorCode": code,
                    "syncToken": "", "moreComing": false, "records": records
                }]}));
            }
            if self.reject_prior {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "serverErrorCode": "BAD_REQUEST", "reason": "private-owner private-provider-url",
                    "syncToken": "", "moreComing": false
                }]}));
            }
        }
        if token == Some(CONTINUATION) {
            match self.fault {
                Fault::EmptyZonesOnContinuation => return Ok(json!({"zones": []})),
                Fault::NullZonesOnContinuation => return Ok(json!({"zones": null})),
                Fault::NullZoneEntryOnContinuation => return Ok(json!({"zones": [null]})),
                Fault::NullResponseOnContinuation => return Ok(Value::Null),
                _ => {}
            }
            if self.fault == Fault::MultipleZonesOnContinuation {
                return Ok(json!({"zones": [
                    {
                        "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                        "syncToken": "refused-primary-successor", "moreComing": false,
                        "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                    },
                    {
                        "zoneID": {"zoneName": HEALTHY_ZONE, "ownerRecordName": "_defaultOwner"},
                        "syncToken": "refused-healthy-successor", "moreComing": false,
                        "records": [{"recordName":format!("asset-{HEALTHY_MASTER}"), "recordType":"CPLAsset", "deleted":true}]
                    }
                ]}));
            }
            if matches!(
                self.fault,
                Fault::WrongZoneOnContinuation
                    | Fault::MissingZoneNameOnContinuation
                    | Fault::MissingZoneIdOnContinuation
            ) {
                let zone_id = match self.fault {
                    Fault::WrongZoneOnContinuation => {
                        Some(json!({"zoneName": HEALTHY_ZONE, "ownerRecordName": "_defaultOwner"}))
                    }
                    Fault::MissingZoneNameOnContinuation => {
                        Some(json!({"ownerRecordName": "_defaultOwner"}))
                    }
                    _ => None,
                };
                let mut zone = json!({
                    "syncToken": "refused-zone-page", "moreComing": false,
                    "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                });
                if let Some(zone_id) = zone_id {
                    zone["zoneID"] = zone_id;
                }
                return Ok(json!({"zones": [zone]}));
            }
            if self.fault == Fault::WrongOwnerOnContinuation {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "private-foreign-owner"},
                    "syncToken": "refused-owner-page", "moreComing": false,
                    "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                }]}));
            }
            if matches!(
                self.fault,
                Fault::MalformedOwnerOnContinuation | Fault::MalformedOwnerOnOwnerlessContinuation
            ) {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": " \t"},
                    "syncToken": "refused-malformed-owner-page", "moreComing": false,
                    "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                }]}));
            }
            if self.fault == Fault::UnrecognizedZoneErrorOnContinuation {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "serverErrorCode": "FUTURE_ZONE_FAILURE",
                    "reason": "private-provider-reason",
                    "syncToken": "refused-zone-error-page", "moreComing": false,
                    "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                }]}));
            }
            if self.fault == Fault::InvalidReasonOnContinuation {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "refused-reason-page", "moreComing": false,
                    "reason": {"detail": "private-provider-detail"},
                    "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                }]}));
            }
            if matches!(
                self.fault,
                Fault::MissingContinuationFlag | Fault::NonBooleanContinuationFlag
            ) {
                let mut zone = json!({
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "refused-flag-page",
                    "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                });
                if self.fault == Fault::NonBooleanContinuationFlag {
                    zone["moreComing"] = json!("false");
                }
                return Ok(json!({"zones": [zone]}));
            }
            if matches!(
                self.fault,
                Fault::MissingRecordsOnContinuation
                    | Fault::NonArrayRecordsOnContinuation
                    | Fault::NonObjectRecordOnContinuation
            ) {
                let mut zone = json!({
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "refused-records-page", "moreComing": false
                });
                let tombstone =
                    json!({"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true});
                match self.fault {
                    Fault::NonArrayRecordsOnContinuation => zone["records"] = tombstone,
                    Fault::NonObjectRecordOnContinuation => {
                        zone["records"] = json!([tombstone, 42]);
                    }
                    _ => {}
                }
                return Ok(json!({"zones": [zone]}));
            }
            if self.fault == Fault::RecordErrorOnContinuation {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "refused-record-page", "moreComing": false,
                    "records": [
                        {"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true},
                        {"recordName":"refused-record", "recordType":"FutureType",
                         "serverErrorCode":"UNKNOWN_ITEM"}
                    ]
                }]}));
            }
            if self.fault == Fault::ForeignRecordZoneOnContinuation {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "refused-record-scope-page", "moreComing": false,
                    "records": [
                        {"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true},
                        {"recordName":format!("asset-{HEALTHY_MASTER}"), "recordType":"CPLAsset",
                         "deleted":true, "zoneID":{"zoneName":HEALTHY_ZONE,"ownerRecordName":"_defaultOwner"}}
                    ]
                }]}));
            }
            if self.fault == Fault::BlankRecordNameOnContinuation {
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": "refused-record-name-page", "moreComing": false,
                    "records": [
                        {"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true},
                        {"recordName":" \t", "recordType":"CPLAsset", "deleted":true}
                    ]
                }]}));
            }
            if matches!(
                self.fault,
                Fault::RepeatedContinuation | Fault::CyclicContinuation
            ) {
                assert!(
                    !self.injected.swap(true, Ordering::SeqCst),
                    "kei continued replay after a repeated cursor"
                );
                let successor = if self.fault == Fault::RepeatedContinuation {
                    CONTINUATION
                } else {
                    PRIOR
                };
                return Ok(json!({"zones": [{
                    "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                    "syncToken": successor, "moreComing": true,
                    "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
                }]}));
            }
            let code = match self.fault {
                Fault::RejectedContinuation | Fault::RejectedContinuationAfterMedia(_) => {
                    Some("BAD_REQUEST")
                }
                Fault::ZoneNotFoundOnContinuation => Some("ZONE_NOT_FOUND"),
                Fault::RetryLaterOnContinuation => Some("RETRY_LATER"),
                Fault::ServerErrorOnContinuation => Some("SERVER_INTERNAL_ERROR"),
                Fault::ThrottledOnContinuation => Some("THROTTLED"),
                Fault::MissingContinuation => None,
                Fault::AuthContinuation => {
                    return Err(
                        crate::icloud::error::ICloudError::SessionExpired { status: 401 }.into(),
                    );
                }
                other => panic!("unexpected continuation in {other:?}"),
            };
            return Ok(json!({"zones": [{
                "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                "serverErrorCode": code, "moreComing": false,
                "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
            }]}));
        }
        let records = if token.is_none() {
            self.inventories.fetch_add(1, Ordering::SeqCst);
            self.records.as_ref().clone()
        } else {
            Vec::new()
        };
        Ok(json!({"zones": [{
            "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
            "syncToken": if token.is_none() {"fresh-source-anchor"} else {"validated-old-replay"},
            "moreComing": false, "records": records
        }]}))
    }
    async fn post_changes_body(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        if let Some((client, endpoint)) = &self.http_continuation
            && url.contains("/changes/zone?")
            && serde_json::from_str::<Value>(&body)?["zones"][0]["syncToken"] == CONTINUATION
        {
            self.requests.fetch_add(1, Ordering::SeqCst);
            return client.post_changes_body(endpoint, body, headers).await;
        }
        Ok(serde_json::to_vec(&self.post(url, body, headers).await?)?)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

async fn seed_sparse(db: &state::SqliteStateDb, source: &str) {
    let evidence = state::SparseEvidence::new(
        r#"[1,"private-child","SharedSync-absent","private-owner"]"#.to_owned(),
    );
    let row = db
        .observe_sparse_identity(
            ZONE,
            &state::SparseSourceId::new(source),
            &evidence,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
    db.record_sparse_attempt(
        &row,
        state::SparseAttemptOutcome::Unresolved(evidence),
        chrono::Utc::now(),
    )
    .await
    .unwrap();
}

fn rows(database: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let connection = rusqlite::Connection::open(database).unwrap();
    let mut statement = connection.prepare(sql).unwrap();
    let width = statement.column_count();
    statement
        .query_map([], |row| (0..width).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

struct Fixture {
    _root: tempfile::TempDir,
    database: PathBuf,
    media: PathBuf,
    kept: PathBuf,
    records: Arc<Vec<Value>>,
    original: Vec<Vec<rusqlite::types::Value>>,
    original_revisions: Vec<Vec<rusqlite::types::Value>>,
    original_mappings: Vec<Vec<rusqlite::types::Value>>,
    original_retries: Vec<Vec<rusqlite::types::Value>>,
    original_sparse: Vec<Vec<rusqlite::types::Value>>,
    transfer_history: Vec<Vec<rusqlite::types::Value>>,
    bytes: Vec<u8>,
    modified: std::time::SystemTime,
}

impl Fixture {
    async fn new(trigger: Trigger) -> Self {
        Self::new_with_owner(trigger, None).await
    }
    async fn new_with_owner(
        trigger: Trigger,
        owner: Option<&state::db::account::AccountOwner>,
    ) -> Self {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let kept = media.join("historical.jpg");
        let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg").to_vec();
        std::fs::write(&kept, &bytes).unwrap();
        std::fs::write(media.join("historical.jpg.xmp"), b"historical sidecar").unwrap();
        let companion = media.join("historical.MOV");
        let companion_bytes = b"historical companion media bytes";
        std::fs::write(&companion, companion_bytes).unwrap();
        let modified = std::fs::metadata(&kept).unwrap().modified().unwrap();
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes));
        let mut records = Vec::new();
        {
            let db = if let Some(owner) = owner {
                Arc::new(
                    state::SqliteStateDb::open_owned(&database, owner)
                        .await
                        .unwrap(),
                )
            } else {
                open_owned_cycle_db(&database).await
            };
            let date = chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS).unwrap();
            let historical_created =
                chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS - 86_400_123)
                    .unwrap();
            let id = if trigger == Trigger::Legacy {
                "legacy-master"
            } else {
                "asset-HISTORY"
            };
            let mut metadata = state::AssetMetadata::default();
            metadata.refresh_hash();
            let record = crate::test_helpers::TestAssetRecord::new(id)
                .filename("historical.jpg")
                .created_at(historical_created)
                .added_at(date)
                .checksum(&checksum)
                .size(bytes.len() as u64)
                .metadata(metadata.clone())
                .build();
            db.upsert_seen(&record).await.unwrap();
            let local = download::file::compute_sha256(&kept).await.unwrap();
            db.mark_downloaded(ZONE, id, "original", &kept, &local, Some(&local))
                .await
                .unwrap();
            let companion_checksum =
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(companion_bytes));
            let companion_local = download::file::compute_sha256(&companion).await.unwrap();
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new(id)
                    .version_size(state::VersionSizeKey::LiveOriginal)
                    .filename("historical.MOV")
                    .created_at(historical_created)
                    .added_at(date)
                    .checksum(&companion_checksum)
                    .size(companion_bytes.len() as u64)
                    .metadata(metadata.clone())
                    .build(),
            )
            .await
            .unwrap();
            db.mark_downloaded(
                ZONE,
                id,
                "live_original",
                &companion,
                &companion_local,
                Some(&companion_local),
            )
            .await
            .unwrap();
            if trigger == Trigger::Legacy {
                db.set_metadata_capture_revision_for_test(ZONE, id, 0);
                let mut page = full_album_page_with_download(
                    ZONE,
                    id,
                    "fresh-rank-anchor",
                    "https://p01.icloud-content.com/private-current.jpg",
                    bytes.len() as u64,
                    &checksum,
                );
                page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                    json!("_defaultOwner");
                records.push(page["records"][0].clone());
                for child in ["current-child-a", "current-child-b"] {
                    db.upsert_asset_master_mapping(ZONE, child, id)
                        .await
                        .unwrap();
                    let mut source = page["records"][1].clone();
                    source["recordName"] = json!(child);
                    records.push(source);
                    let child_path = media.join(format!("{child}.jpg"));
                    std::fs::write(&child_path, &bytes).unwrap();
                    let child_record = crate::test_helpers::TestAssetRecord::new(child)
                        .filename("photo.jpg")
                        .created_at(date)
                        .added_at(date)
                        .checksum(&checksum)
                        .size(bytes.len() as u64)
                        .metadata(metadata.clone())
                        .build();
                    db.upsert_seen(&child_record).await.unwrap();
                    db.mark_downloaded(ZONE, child, "original", &child_path, &local, Some(&local))
                        .await
                        .unwrap();
                }
                db.begin_metadata_capture_revision(ZONE, state::METADATA_CAPTURE_REVISION)
                    .await
                    .unwrap();
                let candidate = db
                    .get_metadata_capture_candidates(ZONE, 1, 1)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|candidate| candidate.asset_id.as_str() == id)
                    .unwrap();
                assert!(
                    db.defer_metadata_capture_ambiguity(&candidate, 1)
                        .await
                        .unwrap()
                );
            } else {
                db.upsert_asset_master_mapping(ZONE, id, "HISTORY")
                    .await
                    .unwrap();
            }
            // Prior transfer failures remain durable historical retry evidence
            // while current policy excludes this non-enumerated identity.
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new("transfer-history")
                    .filename("transfer-history.jpg")
                    .created_at(historical_created)
                    .added_at(date)
                    .checksum("historical-provider-checksum")
                    .size(4096)
                    .build(),
            )
            .await
            .unwrap();
            db.upsert_asset_master_mapping(ZONE, "transfer-history", "transfer-history-master")
                .await
                .unwrap();
            for _ in 0..3 {
                db.mark_failed(
                    ZONE,
                    "transfer-history",
                    "original",
                    "historical transfer failure",
                )
                .await
                .unwrap();
            }
            db.acquire_lock("seed policy-excluded prior transfer history").unwrap().execute(
                "UPDATE assets SET status='pending' WHERE library='PrimarySync' AND id='transfer-history'", []
            ).unwrap();
            assert!(
                db.mark_policy_excluded(ZONE, "transfer-history", "original")
                    .await
                    .unwrap(),
                "fixture must seed an actual excluded row before preservation is asserted"
            );
            db.acquire_lock("seed retained transfer error history").unwrap().execute(
                "UPDATE assets SET last_error='historical transfer failure' WHERE library='PrimarySync' AND id='transfer-history'", []
            ).unwrap();
            // Certification is retired. This legacy-preservation variant still
            // exercises expired retained replay under an actual current
            // configuration reconciliation, not a historical-proof veto.
            let hash = if trigger == Trigger::Sparse {
                download::compute_config_hash(&make_run_cycle_config())
            } else {
                "old-enumeration-generation".to_owned()
            };
            db.set_metadata(ENUM_CONFIG_HASH_KEY, &hash).await.unwrap();
            db.set_metadata(CURSOR_KEY, PRIOR).await.unwrap();
            if matches!(trigger, Trigger::Eligibility | Trigger::Legacy) {
                // These controls have a current delta that still needs replay.
                // Configuration drift alone is not a reason to retain a rejected cursor.
                db.set_metadata(&state::unresolved_identity_key(ZONE), "1")
                    .await
                    .unwrap();
            }
            if trigger == Trigger::Sparse {
                seed_sparse(&db, "private-source").await;
                let evidence = state::SparseEvidence::new(
                    r#"[1,"prior-deleted-child","SharedSync-absent","private-owner"]"#.to_owned(),
                );
                let row = db
                    .observe_sparse_identity(
                        ZONE,
                        &state::SparseSourceId::new("prior-deleted-source"),
                        &evidence,
                        chrono::Utc::now(),
                    )
                    .await
                    .unwrap();
                db.record_sparse_attempt(
                    &row,
                    state::SparseAttemptOutcome::SourceDeleted(
                        state::SparseDeletionCheckpoint::new(PRIOR).unwrap(),
                    ),
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
                db.acquire_lock("defer saved deletion uncertainty").unwrap().execute(
                    "UPDATE unresolved_sparse_identities SET next_retry_at=?1 WHERE library='PrimarySync' AND source_record_name='prior-deleted-source'", [chrono::Utc::now().timestamp()+3600]
                ).unwrap();
            }
        }
        let original = rows(
            &database,
            "SELECT id, library, version_size, filename, created_at, added_at, status, local_path, checksum, local_checksum FROM assets WHERE id IN ('legacy-master','asset-HISTORY') ORDER BY id,version_size",
        );
        let original_revisions = rows(
            &database,
            "SELECT * FROM asset_metadata_capture_revisions WHERE asset_id IN ('legacy-master','asset-HISTORY') ORDER BY asset_id",
        );
        let original_mappings = rows(
            &database,
            "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE master_record_name IN ('legacy-master','HISTORY','transfer-history-master') ORDER BY asset_record_name",
        );
        let original_retries = rows(
            &database,
            "SELECT * FROM metadata_capture_retries WHERE asset_id='legacy-master'",
        );
        let original_sparse = rows(
            &database,
            "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name",
        );
        let transfer_history = rows(
            &database,
            "SELECT library,id,version_size,filename,created_at,added_at,checksum,status,download_attempts,last_error FROM assets WHERE id='transfer-history'",
        );
        Self {
            _root: root,
            database,
            media,
            kept,
            records: Arc::new(records),
            original,
            original_revisions,
            original_mappings,
            original_retries,
            original_sparse,
            transfer_history,
            bytes,
            modified,
        }
    }
    fn assert_preserved(&self) {
        assert_eq!(std::fs::read(&self.kept).unwrap(), self.bytes);
        assert_eq!(
            std::fs::read(self.media.join("historical.jpg.xmp")).unwrap(),
            b"historical sidecar"
        );
        assert_eq!(
            std::fs::read(self.media.join("historical.MOV")).unwrap(),
            b"historical companion media bytes"
        );
        assert_eq!(
            std::fs::metadata(&self.kept).unwrap().modified().unwrap(),
            self.modified
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT id, library, version_size, filename, created_at, added_at, status, local_path, checksum, local_checksum FROM assets WHERE id IN ('legacy-master','asset-HISTORY') ORDER BY id,version_size"
            ),
            self.original
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT * FROM asset_metadata_capture_revisions WHERE asset_id IN ('legacy-master','asset-HISTORY') ORDER BY asset_id"
            ),
            self.original_revisions
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT library,asset_record_name,master_record_name FROM asset_master_mappings WHERE master_record_name IN ('legacy-master','HISTORY','transfer-history-master') ORDER BY asset_record_name"
            ),
            self.original_mappings
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT * FROM metadata_capture_retries WHERE asset_id='legacy-master'"
            ),
            self.original_retries
        );
        assert_eq!(
            rows(
                &self.database,
                "SELECT library,id,version_size,filename,created_at,added_at,checksum,status,download_attempts,last_error FROM assets WHERE id='transfer-history'"
            ),
            self.transfer_history
        );
    }
    fn session(
        &self,
        reject_prior: bool,
        fault: Fault,
        cancel: CancellationToken,
    ) -> RetainedSession {
        RetainedSession {
            records: self.records.clone(),
            reject_prior,
            fault,
            database: self.database.clone(),
            cancel,
            queries: Arc::new(AtomicUsize::new(0)),
            rank_starts: Arc::new(AtomicUsize::new(0)),
            requests: Arc::new(AtomicUsize::new(0)),
            lookups: Arc::new(AtomicUsize::new(0)),
            membership_lookups: Arc::new(AtomicUsize::new(0)),
            inventories: Arc::new(AtomicUsize::new(0)),
            retained: Arc::new(AtomicUsize::new(0)),
            injected: Arc::new(AtomicBool::new(false)),
            http_continuation: None,
        }
    }
}

#[tokio::test]
async fn expired_ordinary_cursor_recovers_without_repairing_an_old_hold() {
    let fixture = Fixture::new(Trigger::Rebaseline).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let config = make_run_cycle_config();
    let old_hold = "unreadable hold from an earlier build";
    let unselected_hold = "retained_checkpoint_hold:SharedSync-UNSELECTED";
    {
        let db = open_owned_cycle_db(&fixture.database).await;
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .unwrap();
        db.set_metadata(HOLD_KEY, old_hold).await.unwrap();
        db.set_metadata(unselected_hold, old_hold).await.unwrap();
    }
    for (fault, expected_cursor) in [
        (Fault::Partial, PRIOR),
        (Fault::None, "fresh-rank-anchor"),
        (Fault::None, "validated-old-replay"),
    ] {
        let db = open_owned_cycle_db(&fixture.database).await;
        let cancel = CancellationToken::new();
        let provider = fixture.session(true, fault, cancel.clone());
        let mut library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
        );
        library.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(provider.clone()),
            ZONE,
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(expected_cursor),
            "{result:?}"
        );
        let quiet = expected_cursor == "validated-old-replay";
        assert_eq!(
            provider.retained.load(Ordering::SeqCst),
            usize::from(!quiet)
        );
        assert_eq!(provider.queries.load(Ordering::SeqCst) == 0, quiet);
        let preserved = fault == Fault::Partial;
        assert_eq!(result.db_sync_token_advance_safe, !preserved, "{result:?}");
        if !preserved {
            assert_eq!(result.failed_count, 0, "{result:?}");
            assert!(!result.stats.sync_token_blocked, "{result:?}");
        }
        assert_eq!(
            db.get_metadata(HOLD_KEY).await.unwrap().as_deref(),
            preserved.then_some(old_hold)
        );
        assert_eq!(
            db.get_metadata(unselected_hold).await.unwrap().as_deref(),
            Some(old_hold)
        );
        assert_eq!(
            rows(
                &fixture.database,
                "SELECT value FROM metadata WHERE key GLOB 'selection_inventory_v1:*'"
            )
            .len(),
            usize::from(!preserved)
        );
        fixture.assert_preserved();
    }
}

const ACTIVE_RETRY_QUERY: &str =
    "SELECT * FROM assets WHERE library='PrimarySync' AND id='held-failed-transfer'";

async fn seed_active_failed_transfer(db: &state::SqliteStateDb) {
    db.upsert_seen(
        &crate::test_helpers::TestAssetRecord::new("held-failed-transfer")
            .filename("held-failed-transfer.jpg")
            .checksum("held-provider-checksum")
            .size(8192)
            .build(),
    )
    .await
    .unwrap();
    db.mark_failed(
        ZONE,
        "held-failed-transfer",
        "original",
        "retained failed transfer cause",
    )
    .await
    .unwrap();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayWrite {
    Embedded,
    #[cfg(feature = "xmp")]
    Sidecar,
    Refresh,
    CaptureTime,
    Truncated,
    Disabled,
}

#[tokio::test]
async fn refused_continuation_cannot_authorize_optional_writes() {
    for option in [
        ReplayWrite::Embedded,
        #[cfg(feature = "xmp")]
        ReplayWrite::Sidecar,
        ReplayWrite::Refresh,
        ReplayWrite::CaptureTime,
        ReplayWrite::Truncated,
        ReplayWrite::Disabled,
    ] {
        assert_replay_write_boundary(option, false).await;
    }
}

#[tokio::test]
async fn refused_continuation_recovers_interrupted_media_receipt_after_reopen() {
    for option in [ReplayWrite::Disabled, ReplayWrite::Embedded] {
        assert_replay_write_boundary(option, true).await;
    }
}

async fn assert_replay_write_boundary(option: ReplayWrite, interrupt_receipt: bool) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CHILD: &str = "asset-accepted-page-master";
    const ASSET: &str = "SELECT * FROM assets WHERE library='PrimarySync' AND id='asset-accepted-page-master' ORDER BY version_size";
    const DESTINATION: &str = "SELECT * FROM provider_active_destinations WHERE child='asset-accepted-page-master' ORDER BY generation,pass_key,version_size,path";
    let fixture = Fixture::new(Trigger::Rebaseline).await;
    let (session_dir, shared) = make_shared_session_for_run_cycle().await;
    let config = make_run_cycle_config();
    let server = MockServer::start().await;
    for endpoint in ["/existing.jpg", "/peer.jpg"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
            .expect(1)
            .mount(&server)
            .await;
    }
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&fixture.bytes));
    let pair = |master: &str, endpoint: &str| {
        let mut page = full_album_page_with_download(
            ZONE,
            master,
            PRIOR,
            &format!("{}{endpoint}", server.uri()),
            fixture.bytes.len() as u64,
            &checksum,
        );
        page["records"][0]["fields"]["filenameEnc"]["value"] =
            json!(endpoint.trim_start_matches('/'));
        page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
            json!("_defaultOwner");
        page["records"][1]["fields"]["timeZoneOffset"] = json!({"value": 0, "type": "INT64"});
        page["records"][1]["fields"]["captionEnc"] =
            json!({"value": "Original caption", "type": "STRING"});
        page["records"].as_array_mut().unwrap().split_off(0)
    };
    let mut records = pair(ACCEPTED_MASTER, "/existing.jpg");
    let media = {
        let db = open_owned_cycle_db(&fixture.database).await;
        let provider = MutableCaptureSession {
            zone: ZONE,
            token: PRIOR,
            cancel_after_lookup: None,
            records: Arc::new(std::sync::Mutex::new(records.clone())),
            repair_requests: Default::default(),
        };
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: make_owned_cycle_album(&db, ZONE, "private", Box::new(provider)),
            exclude_ids: Arc::default(),
        };
        let mut initial = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                #[cfg(feature = "xmp")]
                xmp_sidecar: true,
                ..Default::default()
            },
        )(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            ZONE.into(),
        );
        Arc::make_mut(&mut initial).metadata.set_exif_datetime = true;
        Arc::make_mut(&mut initial).metadata.set_exif_description = true;
        let seeded = download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[pass],
            initial,
            download::DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(seeded.stats.downloaded, 1, "{seeded:?}");
        assert!(
            matches!(seeded.outcome, download::DownloadOutcome::Success),
            "{seeded:?}"
        );
        db.set_metadata(CURSOR_KEY, PRIOR).await.unwrap();
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .unwrap();
        seed_active_failed_transfer(&db).await;
        db.get_downloaded_page(0, 10)
            .await
            .unwrap()
            .into_iter()
            .find(|asset| asset.id.as_ref() == CHILD)
            .unwrap()
            .local_path
            .unwrap()
    };
    records[1]["fields"]["assetDate"]["value"] = json!(RUN_CYCLE_ASSET_DATE_MS + 86_400_000);
    records[1]["fields"]["captionEnc"]["value"] = json!("Changed caption on accepted page");
    records.extend(pair("fresh-peer", "/peer.jpg"));
    let sidecar = media.with_added_extension("xmp");
    #[cfg(not(feature = "xmp"))]
    std::fs::write(&sidecar, b"unmanaged sidecar").unwrap();
    let snapshot = |path: &Path| {
        (
            std::fs::read(path).unwrap(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        )
    };
    let retry = rows(&fixture.database, ACTIVE_RETRY_QUERY);
    let mut published = None;
    assert_eq!(retry.len(), 1);
    for cycle in 0..3 {
        let db = open_owned_cycle_db(&fixture.database).await;
        if cycle == 0 && interrupt_receipt {
            db.acquire_lock("interrupt the peer receipt after publication")
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER interrupt_receipt BEFORE UPDATE OF verified_media ON provider_active_destinations
                     WHEN NEW.child='asset-fresh-peer' AND NEW.verified_media=1
                     BEGIN SELECT RAISE(ABORT,'interrupted media finalization'); END;",
                )
                .unwrap();
        }
        if cycle == 0 && option == ReplayWrite::Truncated {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&media)
                .unwrap()
                .set_len(32)
                .unwrap();
            db.mark_failed(
                ZONE,
                CHILD,
                "original",
                crate::commands::reconcile::FILE_TRUNCATED_REASON,
            )
            .await
            .unwrap();
        }
        let existing = snapshot(&media);
        let sidecar_before = snapshot(&sidecar);
        let asset = rows(&fixture.database, ASSET);
        let destination = rows(&fixture.database, DESTINATION);
        assert_eq!(destination.len(), 1);
        let cancel = CancellationToken::new();
        let mut provider = fixture.session(
            false,
            Fault::RejectedContinuationAfterMedia(AcceptedMedia::Confirmed),
            cancel.clone(),
        );
        provider.records = Arc::new(records.clone());
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions::default(),
        );
        let builder = |mode, excludes, groupings, library| {
            // Force-refresh CLI planning normally chooses full inventory. Test the
            // dispatcher guard when the scoped execution request is incremental.
            let mut scoped = builder(mode, excludes, groupings, library);
            let scoped_config = Arc::make_mut(&mut scoped);
            match option {
                ReplayWrite::Embedded => scoped_config.metadata.set_exif_description = true,
                #[cfg(feature = "xmp")]
                ReplayWrite::Sidecar => scoped_config.metadata.xmp_sidecar = true,
                ReplayWrite::Refresh => scoped_config.refresh_metadata = true,
                ReplayWrite::CaptureTime => {
                    scoped_config.capture_timestamp_repair =
                        download::CaptureTimestampRepair::ReplaceWithCaptureLocal
                }
                ReplayWrite::Truncated => scoped_config.repair_truncated = true,
                ReplayWrite::Disabled => {}
            }
            scoped
        };
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        let receipts = rows(
            &fixture.database,
            "SELECT verified_media FROM provider_active_destinations WHERE child='asset-fresh-peer'",
        );
        if cycle == 0 && !interrupt_receipt {
            assert_eq!(result.stats.downloaded, 1, "{option:?}: {result:?}");
            assert!(provider.lookups.load(Ordering::SeqCst) > 0, "{option:?}");
        }
        let interrupted = cycle == 0 && interrupt_receipt;
        assert_eq!(
            receipts,
            [[rusqlite::types::Value::Integer(i64::from(!interrupted))]],
            "{option:?}"
        );
        let peer: String = db.acquire_lock("read the peer's reserved path").unwrap()
            .query_row("SELECT compat_path FROM provider_active_destinations WHERE child='asset-fresh-peer'", [], |row| row.get(0))
            .unwrap();
        let peer = Path::new(&peer);
        assert!(
            !peer.with_added_extension("xmp").try_exists().unwrap(),
            "{option:?}"
        );
        let installed = snapshot(peer);
        assert_eq!(installed.0, fixture.bytes, "{option:?}");
        if cycle == 0 {
            published = Some(installed);
        } else {
            assert_eq!(Some(installed), published, "{option:?}");
        }
        if interrupted {
            db.acquire_lock("allow finalization on reopen")
                .unwrap()
                .execute_batch("DROP TRIGGER interrupt_receipt")
                .unwrap();
        }
        assert!(
            !result.can_advance_database_checkpoint(),
            "{option:?}: {result:?}"
        );
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some("icloud_changes_continuation_rejected")
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        assert_eq!(provider.retained.load(Ordering::SeqCst), 1);
        assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
        assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
        assert_eq!(snapshot(&media), existing, "{option:?}");
        assert_eq!(snapshot(&sidecar), sidecar_before, "{option:?}");
        assert_eq!(rows(&fixture.database, ASSET), asset, "{option:?}");
        assert_eq!(
            rows(&fixture.database, DESTINATION),
            destination,
            "{option:?}"
        );
        assert_eq!(
            rows(&fixture.database, ACTIVE_RETRY_QUERY),
            retry,
            "{option:?}"
        );
        fixture.assert_preserved();
    }
    drop(session_dir);
    server.verify().await;
}

#[tokio::test]
async fn missing_zone_successor_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::MissingSuccessor).await;
}

#[tokio::test]
async fn missing_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::MissingContinuation).await;
}

#[tokio::test]
async fn rejected_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::RejectedContinuation).await;
}

#[tokio::test]
async fn downloads_accepted_page_media_and_healthy_library_after_continuation_refusal() {
    assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
        AcceptedMedia::Confirmed,
    ))
    .await;
}

#[tokio::test]
async fn continuation_refusal_does_not_publish_an_asset_changed_after_capture() {
    assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
        AcceptedMedia::ChangedOnLookup,
    ))
    .await;
}

#[tokio::test]
async fn named_album_photo_with_current_membership_downloads_after_continuation_refusal() {
    assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
        AcceptedMedia::CurrentAlbumMember,
    ))
    .await;
}

#[tokio::test]
async fn named_album_photo_with_removed_membership_stays_unwritten_after_continuation_refusal() {
    assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
        AcceptedMedia::RemovedAlbumMember,
    ))
    .await;
}

#[tokio::test]
async fn named_album_photo_with_wrong_current_container_stays_unwritten_after_continuation_refusal()
{
    assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
        AcceptedMedia::MovedAlbumMember,
    ))
    .await;
}

#[tokio::test]
async fn mixed_albums_save_verified_photo_and_defer_unresolved_peer_after_continuation_refusal() {
    assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
        AcceptedMedia::MixedAlbums,
    ))
    .await;
}

#[tokio::test]
async fn overlapping_albums_each_require_a_current_link_after_continuation_refusal() {
    for case in [
        AcceptedMedia::OverlappingAlbums,
        AcceptedMedia::OverlapWithRemovedLink,
    ] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(case)).await;
    }
}

#[tokio::test]
async fn mixed_unfiled_requires_a_confirmed_removal_after_continuation_refusal() {
    for case in [
        AcceptedMedia::ConfirmedUnfiled,
        AcceptedMedia::InconclusiveUnfiled,
    ] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(case)).await;
    }
}

#[tokio::test]
async fn mixed_unfiled_with_two_albums_requires_both_current_removals() {
    for missing in [None, Some(REMOVED_LINK), Some(SECOND_REMOVED_LINK)] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
            AcceptedMedia::TwoAlbumRemovals { missing },
        ))
        .await;
    }
}

#[tokio::test]
async fn accepted_child_without_master_requires_the_exact_current_pair() {
    for lookup in [
        MissingMasterLookup::Confirmed,
        MissingMasterLookup::Omitted,
        MissingMasterLookup::ChangedChild,
    ] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
            AcceptedMedia::MissingMaster(lookup),
        ))
        .await;
    }
}

#[tokio::test]
async fn retained_named_album_link_requires_current_membership_when_accepted_page_omits_it() {
    for lookup in [
        RetainedLinkLookup::Current,
        RetainedLinkLookup::Omitted,
        RetainedLinkLookup::WrongAlbum,
    ] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
            AcceptedMedia::RetainedLink(lookup),
        ))
        .await;
    }
}

#[tokio::test]
async fn accepted_favorite_requires_explicit_current_smart_folder_membership() {
    for lookup in [
        FavoriteLookup::Current,
        FavoriteLookup::Missing,
        FavoriteLookup::Unmarked,
        FavoriteLookup::Malformed,
    ] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
            AcceptedMedia::Favorites(lookup),
        ))
        .await;
    }
}

#[tokio::test]
async fn mixed_favorites_unfiled_replay_checks_each_destination_independently() {
    for (unfiled_first, favorite, same_folder) in [
        (false, FavoriteLookup::Current, false),
        (true, FavoriteLookup::Current, false),
        (false, FavoriteLookup::Unmarked, false),
        (false, FavoriteLookup::Malformed, false),
        (true, FavoriteLookup::Current, true),
    ] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
            AcceptedMedia::FavoritesUnfiled {
                unfiled_first,
                favorite,
                same_folder,
            },
        ))
        .await;
    }
}

#[tokio::test]
async fn foreign_album_replay_requires_current_link_and_exact_bound_media() {
    for lookup in [
        ForeignLookup::Current,
        ForeignLookup::MissingLink,
        ForeignLookup::UnknownOwnerItem,
        ForeignLookup::OwnerNamesake,
        ForeignLookup::MissingMaster,
        ForeignLookup::WrongMediaScope,
    ] {
        assert_zone_replay_isolation(Fault::RejectedContinuationAfterMedia(
            AcceptedMedia::ForeignAlbum(lookup),
        ))
        .await;
    }
}

fn replay_album_link(relation: &str, container: &str, master: &str) -> Value {
    json!({
        "recordName": relation,
        "recordType": "CPLContainerRelation",
        "recordChangeTag": "relation-before-continuation",
        "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
        "fields": {
            "containerId": {"value": container, "type": "STRING"},
            "itemId": {"value": format!("asset-{master}"), "type": "STRING"}
        }
    })
}

#[tokio::test]
async fn wrong_owner_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::WrongOwnerOnContinuation).await;
}

#[tokio::test]
async fn malformed_owner_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::MalformedOwnerOnContinuation).await;
}

#[tokio::test]
async fn malformed_owner_on_ownerless_continuation_is_scoped_before_capture() {
    use crate::icloud::photos::error::saved_cursor_rejected;
    use rusqlite::types::Value as SqlValue;
    use tokio_stream::StreamExt;

    let fixture = Fixture::new(Trigger::Rebaseline).await;
    let db = open_owned_cycle_db(&fixture.database).await;
    seed_active_failed_transfer(&db).await;
    let retry = rows(&fixture.database, ACTIVE_RETRY_QUERY);
    assert_eq!(retry.len(), 1);
    let provider = fixture.session(
        false,
        Fault::MalformedOwnerOnOwnerlessContinuation,
        CancellationToken::new(),
    );
    let album = make_owned_cycle_album_without_request_owner(&db, ZONE, Box::new(provider.clone()));
    let (mut stream, last_accepted, completion) = album.changes_stream_with_completion(PRIOR);
    let error = stream.next().await.unwrap().unwrap_err();
    assert_eq!(
        crate::download::replay_refusal_reason(&error),
        Some("icloud_changes_invalid_zone_identity"),
        "{error:#}"
    );
    assert!(!saved_cursor_rejected(&error));
    assert!(stream.next().await.is_none());
    assert_eq!(last_accepted.await.unwrap(), CONTINUATION);
    assert!(completion.await.is_err());
    assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
    assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
    assert_eq!(rows(&fixture.database, ACTIVE_RETRY_QUERY), retry);
    assert_eq!(
        db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
        Some(PRIOR)
    );
    let pages = rows(
        &fixture.database,
        "SELECT request_cursor, successor FROM provider_shadow_pages",
    );
    assert_eq!(
        pages,
        [vec![
            SqlValue::Text(PRIOR.into()),
            SqlValue::Text(CONTINUATION.into())
        ]]
    );
    fixture.assert_preserved();
}

#[tokio::test]
async fn wrong_zone_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::WrongZoneOnContinuation).await;
}

#[tokio::test]
async fn missing_zone_name_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::MissingZoneNameOnContinuation).await;
}

#[tokio::test]
async fn missing_zone_id_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::MissingZoneIdOnContinuation).await;
}

#[tokio::test]
async fn empty_zones_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::EmptyZonesOnContinuation).await;
}

#[tokio::test]
async fn multiple_zones_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::MultipleZonesOnContinuation).await;
}

#[tokio::test]
async fn null_zones_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::NullZonesOnContinuation).await;
}

#[tokio::test]
async fn null_zone_entry_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::NullZoneEntryOnContinuation).await;
}

#[tokio::test]
async fn null_response_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::NullResponseOnContinuation).await;
}

#[tokio::test]
async fn malformed_json_on_http_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::MalformedJsonOnContinuation).await;
}

#[tokio::test]
async fn success_shaped_errors_cannot_replace_a_saved_checkpoint() {
    for fault in [Fault::MalformedResponseError, Fault::RedirectOnContinuation] {
        assert_zone_replay_isolation(fault).await;
    }
}

#[tokio::test]
async fn duplicate_json_keys_on_http_continuation_preserve_retry_and_allow_later_library_download()
{
    assert_zone_replay_isolation(Fault::DuplicateJsonKeysOnContinuation).await;
}

#[tokio::test]
async fn oversized_body_on_http_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::OversizedBodyOnContinuation).await;
}

#[tokio::test]
async fn repeated_or_cyclic_continuation_preserves_retry_and_allows_later_library_download() {
    for fault in [Fault::RepeatedContinuation, Fault::CyclicContinuation] {
        assert_zone_replay_isolation(fault).await;
    }
}

#[tokio::test]
async fn record_error_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::RecordErrorOnContinuation).await;
}

#[tokio::test]
async fn foreign_record_zone_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::ForeignRecordZoneOnContinuation).await;
}

#[tokio::test]
async fn blank_record_name_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::BlankRecordNameOnContinuation).await;
}

#[tokio::test]
async fn invalid_records_on_continuation_preserves_retry_and_allows_later_library_download() {
    for fault in [
        Fault::MissingRecordsOnContinuation,
        Fault::NonArrayRecordsOnContinuation,
    ] {
        assert_zone_replay_isolation(fault).await;
    }
}

#[tokio::test]
async fn non_object_record_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::NonObjectRecordOnContinuation).await;
}

#[tokio::test]
async fn invalid_continuation_flag_preserves_retry_and_allows_later_library_download() {
    for fault in [
        Fault::MissingContinuationFlag,
        Fault::NonBooleanContinuationFlag,
    ] {
        assert_zone_replay_isolation(fault).await;
    }
}

#[tokio::test]
async fn invalid_reason_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::InvalidReasonOnContinuation).await;
}

#[tokio::test]
async fn missing_zone_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::ZoneNotFoundOnContinuation).await;
}

#[tokio::test]
async fn retry_later_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::RetryLaterOnContinuation).await;
}

#[tokio::test]
async fn server_error_on_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::ServerErrorOnContinuation).await;
}

#[tokio::test]
async fn unrecognized_zone_error_on_continuation_preserves_retry_and_allows_later_library_download()
{
    assert_zone_replay_isolation(Fault::UnrecognizedZoneErrorOnContinuation).await;
}

#[tokio::test]
async fn throttled_continuation_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::ThrottledOnContinuation).await;
}

#[tokio::test]
async fn throttled_first_replay_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::ThrottledOnFirstReplay).await;
}

#[tokio::test]
async fn retry_later_on_first_replay_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::RetryLaterOnFirstReplay).await;
}

#[tokio::test]
async fn server_error_on_first_replay_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::ServerErrorOnFirstReplay).await;
}

#[tokio::test]
async fn missing_zone_on_first_replay_preserves_retry_and_allows_later_library_download() {
    assert_zone_replay_isolation(Fault::ZoneNotFoundOnFirstReplay).await;
}

#[tokio::test]
async fn authentication_failure_on_continuation_stops_later_library_download() {
    assert_zone_replay_isolation(Fault::AuthContinuation).await;
}

async fn assert_zone_replay_isolation(fault: Fault) {
    Box::pin(check_replay_with_prior_album(fault, None)).await;
}

#[tokio::test]
async fn verified_prior_album_preserves_its_copy_while_new_destinations_publish() {
    for media in [
        AcceptedMedia::OverlapWithRemovedLink,
        AcceptedMedia::FavoritesUnfiled {
            unfiled_first: true,
            favorite: FavoriteLookup::Current,
            same_folder: false,
        },
        AcceptedMedia::TwoAlbumRemovals { missing: None },
        AcceptedMedia::TwoAlbumRemovals {
            missing: Some(REMOVED_LINK),
        },
        AcceptedMedia::TwoAlbumRemovals {
            missing: Some(SECOND_REMOVED_LINK),
        },
        AcceptedMedia::ForeignAlbum(ForeignLookup::Current),
        AcceptedMedia::ForeignAlbum(ForeignLookup::WrongMediaScope),
        AcceptedMedia::FavoritesUnfiled {
            unfiled_first: false,
            favorite: FavoriteLookup::Unmarked,
            same_folder: false,
        },
    ] {
        Box::pin(check_replay_with_prior_album(
            Fault::RejectedContinuationAfterMedia(media),
            Some(if matches!(media, AcceptedMedia::ForeignAlbum(_)) {
                "shared"
            } else {
                "private"
            }),
        ))
        .await;
    }
    // The zone name alone does not identify a database. A namesake private
    // receipt must not authorize reuse of the catalog row for shared media.
    Box::pin(check_replay_with_prior_album(
        Fault::RejectedContinuationAfterMedia(AcceptedMedia::ForeignAlbum(ForeignLookup::Current)),
        Some("private"),
    ))
    .await;
}

async fn check_replay_with_prior_album(fault: Fault, prior_database: Option<&str>) {
    use base64::Engine as _;
    use rusqlite::types::Value as SqlValue;
    use sha2::{Digest, Sha256};
    use tokio_stream::StreamExt;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, ResponseTemplate};

    const HEALTHY_KEY: &str = "sync_token:SharedSync-CLEAN";
    let session_expired = fault == Fault::AuthContinuation;
    let first_page_refusal =
        fault == Fault::MissingSuccessor || fault.first_replay_zone_error().is_some();
    let fixture = Fixture::new(Trigger::Rebaseline).await;
    let config = make_run_cycle_config();
    let db = open_owned_cycle_db(&fixture.database).await;
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .unwrap();
    db.set_metadata(HEALTHY_KEY, "healthy-before")
        .await
        .unwrap();
    seed_active_failed_transfer(&db).await;
    let retained_link = matches!(
        fault,
        Fault::RejectedContinuationAfterMedia(AcceptedMedia::RetainedLink(_))
    );
    if retained_link {
        let album = make_owned_cycle_container_album(
            &db,
            ZONE,
            ACCEPTED_ALBUM,
            ACCEPTED_CONTAINER,
            Box::new(fixture.session(false, fault, CancellationToken::new())),
        );
        let (mut stream, last, completion) =
            album.changes_stream_with_completion(RETAINED_LINK_CURSOR);
        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.record_name.as_ref(), ACCEPTED_RELATION);
        assert!(stream.next().await.is_none());
        assert_eq!(last.await.unwrap(), "earlier-album-link-successor");
        completion.await.unwrap();
        assert_eq!(
            db.acquire_lock("make the historical raw link body unreadable")
                .unwrap()
                .execute(
                    "UPDATE provider_shadow_pages SET body=X'7B' WHERE request_cursor=?1",
                    [RETAINED_LINK_CURSOR],
                )
                .unwrap(),
            1
        );
    }
    drop(db);
    let retry = rows(&fixture.database, ACTIVE_RETRY_QUERY);
    assert_eq!(retry.len(), 1);

    let server = wiremock::MockServer::start().await;
    let mixed_albums = fault == Fault::RejectedContinuationAfterMedia(AcceptedMedia::MixedAlbums);
    let overlap = matches!(fault, Fault::RejectedContinuationAfterMedia(case) if case.overlaps());
    let missing_master = matches!(
        fault,
        Fault::RejectedContinuationAfterMedia(AcceptedMedia::MissingMaster(_))
    );
    let favorites = matches!(
        fault,
        Fault::RejectedContinuationAfterMedia(AcceptedMedia::Favorites(_))
    );
    let favorite_mix = match fault {
        Fault::RejectedContinuationAfterMedia(AcceptedMedia::FavoritesUnfiled {
            unfiled_first,
            favorite,
            same_folder,
        }) => Some((unfiled_first, favorite, same_folder)),
        _ => None,
    };
    let foreign = match fault {
        Fault::RejectedContinuationAfterMedia(AcceptedMedia::ForeignAlbum(result)) => Some(result),
        _ => None,
    };
    let mixed_unfiled = matches!(fault, Fault::RejectedContinuationAfterMedia(case) if case.tests_unfiled_removal());
    let two_album_removals = matches!(
        fault,
        Fault::RejectedContinuationAfterMedia(AcceptedMedia::TwoAlbumRemovals { .. })
    );
    let second_copy = fault
        == Fault::RejectedContinuationAfterMedia(AcceptedMedia::OverlappingAlbums)
        || favorite_mix.is_some_and(|(_, favorite, _)| favorite == FavoriteLookup::Current);
    let named_album = matches!(
        fault,
        Fault::RejectedContinuationAfterMedia(
            AcceptedMedia::CurrentAlbumMember
                | AcceptedMedia::RemovedAlbumMember
                | AcceptedMedia::MovedAlbumMember
                | AcceptedMedia::MixedAlbums
                | AcceptedMedia::OverlappingAlbums
                | AcceptedMedia::OverlapWithRemovedLink
                | AcceptedMedia::ConfirmedUnfiled
                | AcceptedMedia::InconclusiveUnfiled
                | AcceptedMedia::TwoAlbumRemovals { .. }
                | AcceptedMedia::RetainedLink(_)
                | AcceptedMedia::ForeignAlbum(_)
        )
    );
    let verified_accepted = (foreign.is_none() || prior_database != Some("private"))
        && matches!(
            fault,
            Fault::RejectedContinuationAfterMedia(
                AcceptedMedia::Confirmed
                    | AcceptedMedia::CurrentAlbumMember
                    | AcceptedMedia::MixedAlbums
                    | AcceptedMedia::OverlappingAlbums
                    | AcceptedMedia::OverlapWithRemovedLink
                    | AcceptedMedia::ConfirmedUnfiled
                    | AcceptedMedia::TwoAlbumRemovals { missing: None }
                    | AcceptedMedia::MissingMaster(MissingMasterLookup::Confirmed)
                    | AcceptedMedia::RetainedLink(RetainedLinkLookup::Current)
                    | AcceptedMedia::Favorites(FavoriteLookup::Current)
                    | AcceptedMedia::FavoritesUnfiled { .. }
                    | AcceptedMedia::ForeignAlbum(ForeignLookup::Current)
            )
        );
    let accepted_media = if matches!(fault, Fault::RejectedContinuationAfterMedia(_)) {
        let bytes = include_bytes!("../../../../tests/data/media/metadata.jpg").to_vec();
        let checksum = Sha256::digest(&bytes);
        Mock::given(method("GET"))
            .and(path("/accepted.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .expect(
                u64::from(verified_accepted)
                    + u64::from(second_copy)
                    + u64::from(prior_database.is_some()),
            )
            .mount(&server)
            .await;
        let mut page = full_album_page_with_download(
            ZONE,
            ACCEPTED_MASTER,
            CONTINUATION,
            &format!("{}/accepted.jpg", server.uri()),
            bytes.len() as u64,
            &base64::engine::general_purpose::STANDARD.encode(checksum),
        );
        page["records"][0]["fields"]["filenameEnc"]["value"] = json!("accepted.jpg");
        page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
            json!("_defaultOwner");
        if favorites || favorite_mix.is_some() {
            let marked =
                favorite_mix.is_none_or(|(_, favorite, _)| favorite != FavoriteLookup::Unmarked);
            page["records"][1]["fields"]["isFavorite"] =
                json!({"value":i64::from(marked),"type":"INT64"});
        }
        let mut records = page["records"].as_array().unwrap().clone();
        if mixed_unfiled {
            for link in [REMOVED_LINK, SECOND_REMOVED_LINK]
                .into_iter()
                .take(1 + usize::from(two_album_removals))
            {
                records.push(
                    json!({"recordName": link, "recordType": null, "deleted": true,
                    "zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"}}),
                );
            }
        } else if named_album {
            records.push(replay_album_link(
                ACCEPTED_RELATION,
                ACCEPTED_CONTAINER,
                ACCEPTED_MASTER,
            ));
        }
        if mixed_albums {
            Mock::given(method("GET"))
                .and(path("/unresolved.jpg"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
                .expect(0)
                .mount(&server)
                .await;
            let mut page = full_album_page_with_download(
                ZONE,
                UNRESOLVED_MASTER,
                CONTINUATION,
                &format!("{}/unresolved.jpg", server.uri()),
                bytes.len() as u64,
                &base64::engine::general_purpose::STANDARD.encode(checksum),
            );
            page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                json!("_defaultOwner");
            records.append(page["records"].as_array_mut().unwrap());
            records.push(replay_album_link(
                UNRESOLVED_RELATION,
                UNRESOLVED_CONTAINER,
                UNRESOLVED_MASTER,
            ));
        }
        if overlap {
            records.push(replay_album_link(
                UNRESOLVED_RELATION,
                UNRESOLVED_CONTAINER,
                ACCEPTED_MASTER,
            ));
        }
        Some((bytes, Arc::new(records)))
    } else {
        None
    };
    let refused_body = match fault {
        Fault::MalformedResponseError | Fault::RedirectOnContinuation => {
            let mut body = json!({"zones":[{
                "zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},
                "records":[{"recordName":"asset-HISTORY","deleted":true}],
                "syncToken":"untrusted-position","moreComing":false
            }]});
            if fault == Fault::MalformedResponseError {
                body["serverErrorCode"] = json!({"malformed":true});
            }
            Some(serde_json::to_vec(&body).unwrap())
        }
        Fault::MalformedJsonOnContinuation => {
            Some(br#"{"zones":[{"private-truncated-response""#.to_vec())
        }
        Fault::DuplicateJsonKeysOnContinuation => Some(br#"{"zones":[{
            "zoneID":{"zoneName":"SharedSync-CLEAN","zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
            "syncToken":"private-ambiguous-successor","moreComing":false,
            "records":[{"recordName":"asset-HISTORY","recordType":"CPLAsset","deleted":true}]
        }]}"#.to_vec()),
        Fault::OversizedBodyOnContinuation => {
            // Valid JSON at one byte above the cap: size, not decoding, must refuse it.
            let mut body = serde_json::to_vec(&json!({"zones": [{
                "zoneID": {"zoneName": ZONE, "ownerRecordName": "_defaultOwner"},
                "syncToken": "refused-oversized-page", "moreComing": false,
                "records": [{"recordName":"asset-HISTORY", "recordType":"CPLAsset", "deleted":true}]
            }]}))
            .unwrap();
            body.resize(crate::icloud::photos::inbox::MAX_CHANGES_PAGE_BYTES + 1, b' ');
            Some(body)
        }
        _ => None,
    };
    let http_continuation = if let Some(body) = refused_body {
        Mock::given(method("POST"))
            .and(path("/changes/zone"))
            .and(body_string_contains(format!(
                r#""syncToken":"{CONTINUATION}""#
            )))
            .respond_with(
                ResponseTemplate::new(if fault == Fault::RedirectOnContinuation {
                    300
                } else {
                    200
                })
                .set_body_bytes(body)
                .insert_header("Content-Type", "application/json"),
            )
            .expect(3)
            .mount(&server)
            .await;
        Some((
            reqwest::Client::new(),
            format!("{}/changes/zone", server.uri()),
        ))
    } else {
        None
    };
    Mock::given(method("GET"))
        .and(path("/healthy.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture.bytes.clone()))
        .expect(u64::from(!session_expired))
        .mount(&server)
        .await;
    let checksum = Sha256::digest(&fixture.bytes);
    let mut page = full_album_page_with_download(
        HEALTHY_ZONE,
        HEALTHY_MASTER,
        "unused",
        &format!("{}/healthy.jpg", server.uri()),
        fixture.bytes.len() as u64,
        &base64::engine::general_purpose::STANDARD.encode(checksum),
    );
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let healthy_provider = MutableCaptureSession {
        zone: HEALTHY_ZONE,
        token: "healthy-successor",
        cancel_after_lookup: None,
        records: Arc::new(std::sync::Mutex::new(
            page["records"].as_array().unwrap().clone(),
        )),
        repair_requests: Default::default(),
    };
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;

    let old_file = fixture.media.join(UNRESOLVED_ALBUM).join("accepted.JPG");
    const PRIOR_PATH_RECEIPT: &str = "SELECT * FROM provider_active_destinations WHERE compat_path LIKE '%/Unresolved Album/accepted.JPG'";
    let sidecar = old_file.with_added_extension("xmp");
    let snapshot = |p: &Path| {
        (
            std::fs::read(p).unwrap(),
            std::fs::metadata(p).unwrap().modified().unwrap(),
        )
    };
    let prior = if let Some(database) = prior_database {
        let (bytes, records) = accepted_media.as_ref().unwrap();
        let db = open_owned_cycle_db(&fixture.database).await;
        let zone = if foreign.is_some() {
            FOREIGN_ZONE
        } else {
            ZONE
        };
        let mut pair = records[..2].to_vec();
        pair[1]["fields"]["masterRef"]["value"]["zoneID"]["zoneName"] = json!(zone);
        let session = MutableCaptureSession {
            zone,
            token: "prior-good",
            cancel_after_lookup: None,
            records: Arc::new(std::sync::Mutex::new(pair)),
            repair_requests: Default::default(),
        };
        let mut album = make_owned_cycle_container_album(
            &db,
            zone,
            UNRESOLVED_ALBUM,
            UNRESOLVED_CONTAINER,
            Box::new(session),
        );
        let (capture, _, _) = album.current_selection_scope().unwrap().unwrap();
        album.set_shadow_capture(capture, database.into());
        let pass = AlbumPass {
            kind: PassKind::Album,
            album,
            exclude_ids: Arc::default(),
        };
        let cfg = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                per_pass_paths: true,
                ..Default::default()
            },
        )(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            zone.into(),
        );
        let saved = download::download_photos_with_sync(
            &reqwest::Client::new(),
            &[pass],
            cfg,
            download::DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(saved.stats.downloaded, 1);
        assert_eq!(std::fs::read(&old_file).unwrap(), *bytes);
        std::fs::write(&sidecar, b"retained sidecar").unwrap();
        Some((
            snapshot(&old_file),
            snapshot(&sidecar),
            rows(&fixture.database, PRIOR_PATH_RECEIPT),
        ))
    } else {
        None
    };
    for cycle in 0..3 {
        let db = open_owned_cycle_db(&fixture.database).await;
        let cancel = CancellationToken::new();
        let mut provider = fixture.session(false, fault, cancel.clone());
        provider.http_continuation.clone_from(&http_continuation);
        if let Some((_, records)) = &accepted_media {
            provider.records.clone_from(records);
        }
        let foreign_media = foreign.map(|result| ForeignMediaSession {
            records: provider.records.clone(),
            result,
            lookups: Arc::default(),
        });
        let blocked = if named_album {
            let album = |name, container| AlbumPass {
                kind: PassKind::Album,
                album: make_owned_cycle_container_album(
                    &db,
                    ZONE,
                    name,
                    container,
                    Box::new(provider.clone()),
                ),
                exclude_ids: Arc::default(),
            };
            let mut first = album(ACCEPTED_ALBUM, ACCEPTED_CONTAINER);
            if let Some(session) = &foreign_media {
                first.album = first
                    .album
                    .with_cross_zone_sources(vec![make_owned_cycle_album(
                        &db,
                        FOREIGN_ZONE,
                        "shared",
                        Box::new(session.clone()),
                    )]);
            }
            let mut passes = vec![first];
            if mixed_albums {
                passes.insert(0, album(UNRESOLVED_ALBUM, UNRESOLVED_CONTAINER));
            }
            if two_album_removals {
                passes.push(album(UNRESOLVED_ALBUM, UNRESOLVED_CONTAINER));
            }
            if mixed_albums || mixed_unfiled {
                passes.push(AlbumPass {
                    kind: PassKind::Unfiled,
                    album: make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
                    exclude_ids: Arc::default(),
                });
            }
            if overlap {
                passes.push(album(UNRESOLVED_ALBUM, UNRESOLVED_CONTAINER));
            }
            make_run_cycle_library_state_with_passes(ZONE, CURSOR_KEY, passes)
        } else if favorites || favorite_mix.is_some() {
            let mut passes = vec![AlbumPass {
                kind: PassKind::SmartFolder,
                album: make_owned_cycle_smart_folder(
                    &db,
                    ZONE,
                    FAVORITES,
                    Box::new(provider.clone()),
                ),
                exclude_ids: Arc::default(),
            }];
            if let Some((unfiled_first, _, _)) = favorite_mix {
                passes.insert(
                    usize::from(!unfiled_first),
                    AlbumPass {
                        kind: PassKind::Unfiled,
                        album: make_owned_cycle_album(
                            &db,
                            ZONE,
                            "private",
                            Box::new(provider.clone()),
                        ),
                        exclude_ids: Arc::default(),
                    },
                );
            }
            make_run_cycle_library_state_with_passes(ZONE, CURSOR_KEY, passes)
        } else {
            make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
            )
        };
        let healthy = make_run_cycle_library_state_with_album(
            HEALTHY_ZONE,
            HEALTHY_KEY,
            make_owned_cycle_album(
                &db,
                HEALTHY_ZONE,
                "shared",
                Box::new(healthy_provider.clone()),
            ),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                per_pass_paths: named_album,
                ..Default::default()
            },
        );
        let builder = |mode, excludes, groupings, library| {
            let mut config = builder(mode, excludes, groupings, library);
            if favorites || favorite_mix.is_some() {
                Arc::make_mut(&mut config).folder_structure_smart_folders =
                    Arc::from("{smart-folder}");
            }
            if let Some((_, _, same_folder)) = favorite_mix {
                Arc::make_mut(&mut config).folder_structure =
                    if same_folder { FAVORITES } else { "unfiled" }.to_owned();
            }
            config
        };
        let result = run_cycle(
            &[&blocked, &healthy],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            provider.queries.load(Ordering::SeqCst),
            0,
            "a scoped replay refusal must not start an inventory"
        );
        assert!(!result.can_advance_database_checkpoint(), "{result:?}");
        assert_eq!(result.session_expired, session_expired, "{result:?}");
        if !session_expired {
            assert_eq!(result.failed_count, 1, "{result:?}");
            assert_eq!(result.stats.sync_token_blocked_zone.as_deref(), Some(ZONE));
            assert_eq!(
                result.stats.sync_token_blocked_reason,
                Some(match fault {
                    Fault::MissingContinuationFlag | Fault::NonBooleanContinuationFlag => {
                        "icloud_changes_invalid_continuation_flag"
                    }
                    Fault::InvalidReasonOnContinuation => "icloud_changes_invalid_reason",
                    Fault::RejectedContinuation | Fault::RejectedContinuationAfterMedia(_) => {
                        "icloud_changes_continuation_rejected"
                    }
                    Fault::WrongZoneOnContinuation | Fault::MissingZoneNameOnContinuation => {
                        "icloud_changes_zone_name_mismatch"
                    }
                    Fault::MissingZoneIdOnContinuation
                    | Fault::NullZoneEntryOnContinuation
                    | Fault::MalformedOwnerOnContinuation => {
                        "icloud_changes_invalid_zone_identity"
                    }
                    Fault::EmptyZonesOnContinuation => "icloud_changes_empty_zone_results",
                    Fault::MultipleZonesOnContinuation => "icloud_changes_multiple_zone_results",
                    Fault::NullZonesOnContinuation | Fault::NullResponseOnContinuation => {
                        "icloud_changes_invalid_zones_field"
                    }
                    Fault::MalformedJsonOnContinuation => "icloud_changes_malformed_json",
                    Fault::MalformedResponseError | Fault::RedirectOnContinuation =>
                        "icloud_changes_unsuccessful_response",
                    Fault::DuplicateJsonKeysOnContinuation => "icloud_changes_duplicate_json_keys",
                    Fault::OversizedBodyOnContinuation => "icloud_changes_oversized_body",
                    Fault::WrongOwnerOnContinuation => "icloud_changes_zone_owner_mismatch",
                    Fault::RepeatedContinuation | Fault::CyclicContinuation => {
                        "icloud_changes_repeated_continuation"
                    }
                    Fault::RecordErrorOnContinuation => "icloud_changes_record_error",
                    Fault::ForeignRecordZoneOnContinuation =>
                        "icloud_changes_record_scope_mismatch",
                    Fault::MissingRecordsOnContinuation
                    | Fault::BlankRecordNameOnContinuation
                    | Fault::NonArrayRecordsOnContinuation
                    | Fault::NonObjectRecordOnContinuation => "icloud_changes_invalid_records",
                    Fault::ZoneNotFoundOnContinuation | Fault::ZoneNotFoundOnFirstReplay => {
                        "icloud_changes_zone_not_found"
                    }
                    Fault::RetryLaterOnContinuation | Fault::RetryLaterOnFirstReplay => {
                        "icloud_changes_retry_later"
                    }
                    Fault::ServerErrorOnContinuation | Fault::ServerErrorOnFirstReplay => {
                        "icloud_changes_server_internal_error"
                    }
                    Fault::UnrecognizedZoneErrorOnContinuation => {
                        "icloud_changes_unrecognized_zone_error"
                    }
                    Fault::ThrottledOnContinuation | Fault::ThrottledOnFirstReplay => {
                        "icloud_changes_zone_throttled"
                    }
                    _ => "icloud_changes_missing_successor",
                })
            );
        }
        if cycle > 0 {
            assert!(result.stats.full_enumeration_reason.is_none());
            assert_eq!(result.stats.api_total_at_start, None);
        }
        let compacted_rereads =
            usize::from(prior_database.is_some() && favorite_mix.is_some() && cycle == 0);
        assert_eq!(
            provider.retained.load(Ordering::SeqCst),
            1 + compacted_rereads
        );
        assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
        assert_eq!(db.get_metadata(HOLD_KEY).await.unwrap(), None);
        let page_count = 1 + usize::from(!first_page_refusal);
        let lookups = if accepted_media.is_some() {
            provider.lookups.load(Ordering::SeqCst)
        } else {
            0
        };
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            page_count + lookups + compacted_rereads
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        assert_eq!(
            db.get_metadata(HEALTHY_KEY).await.unwrap().as_deref(),
            Some(if session_expired {
                "healthy-before"
            } else {
                "healthy-successor"
            })
        );
        assert_eq!(rows(&fixture.database, ACTIVE_RETRY_QUERY), retry);
        // Check the request cursor too: a foreign page must not be captured under its own zone.
        let captured = rows(
            &fixture.database,
            "SELECT request_cursor,successor,more_coming FROM provider_shadow_pages
             WHERE json_extract(scope,'$.zone.zoneName')='PrimarySync'
                OR request_cursor='accepted-page' ORDER BY id",
        );
        let mut expected_pages = Vec::new();
        if retained_link {
            expected_pages.push(vec![
                SqlValue::Text(RETAINED_LINK_CURSOR.into()),
                SqlValue::Text("earlier-album-link-successor".into()),
                SqlValue::Integer(0),
            ]);
        }
        if !first_page_refusal {
            expected_pages.push(vec![
                SqlValue::Text(PRIOR.into()),
                SqlValue::Text(CONTINUATION.into()),
                SqlValue::Integer(1),
            ]);
        }
        assert_eq!(captured, expected_pages);
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.source_deleted, 0);
        assert_eq!(
            summary.last_recovery_action.as_deref(),
            Some(if session_expired {
                "reauthenticate"
            } else {
                "replay_from_prior_token"
            })
        );
        let installed = db.get_downloaded_page(0, 10).await.unwrap();
        let healthy_rows = installed
            .iter()
            .filter(|row| row.library.as_ref() == HEALTHY_ZONE)
            .collect::<Vec<_>>();
        assert_eq!(healthy_rows.len(), usize::from(!session_expired));
        if let Some(installed) = healthy_rows.first() {
            assert_eq!(
                installed.local_checksum.as_deref(),
                Some(format!("{checksum:x}").as_str())
            );
            assert_eq!(
                tokio::fs::read(installed.local_path.as_ref().unwrap())
                    .await
                    .unwrap(),
                fixture.bytes
            );
        }
        if let Some(prior) = &prior {
            assert_eq!(snapshot(&old_file), prior.0);
            assert_eq!(snapshot(&sidecar), prior.1);
            assert_eq!(rows(&fixture.database, PRIOR_PATH_RECEIPT), prior.2);
            assert_eq!(prior.2.len(), 1);
            let folders: &[&str] = if !verified_accepted {
                &[]
            } else if favorite_mix.is_some() && !second_copy {
                &["unfiled"]
            } else if favorite_mix.is_some() {
                &[FAVORITES, "unfiled"]
            } else if mixed_unfiled {
                &["2023/11/14"]
            } else {
                &[ACCEPTED_ALBUM]
            };
            let gets = server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.url.path() == "/accepted.jpg")
                .count();
            assert_eq!(gets, 1 + folders.len(), "{fault:?} cycle {cycle}");
            for folder in [FAVORITES, "unfiled", "2023/11/14", ACCEPTED_ALBUM] {
                assert_eq!(
                    fixture.media.join(folder).join("accepted.JPG").exists(),
                    folders.contains(&folder),
                    "{fault:?} cycle {cycle}: {folder}"
                );
            }
            for folder in folders {
                let path = fixture.media.join(folder).join("accepted.JPG");
                assert_eq!(
                    std::fs::read(&path).unwrap_or_else(|e| panic!("{fault:?} {path:?}: {e}")),
                    prior.0.0
                );
                let conn = rusqlite::Connection::open(&fixture.database).unwrap();
                assert_eq!(conn.query_row("SELECT COUNT(*) FROM provider_active_destinations WHERE compat_path=?1 AND verified_media=1 AND child='asset-accepted-page-master'",[path.to_str().unwrap()],|r| r.get::<_,i64>(0)).unwrap(),1);
            }
            assert_eq!(
                result.stats.downloaded,
                usize::from(cycle == 0) * (1 + folders.len()),
                "{fault:?}"
            );
            fixture.assert_preserved();
            continue;
        }
        if let Some((bytes, _)) = &accepted_media {
            let child = format!("asset-{ACCEPTED_MASTER}");
            if cycle == 0 {
                assert!(lookups > 0, "publication needs fresh paired confirmation");
                if named_album {
                    assert!(provider.membership_lookups.load(Ordering::SeqCst) > 0);
                }
            }
            let mut expected_records = vec![
                vec![SqlValue::Text(ACCEPTED_MASTER.into())],
                vec![SqlValue::Text(child.clone())],
            ];
            if let Some(media) = &foreign_media {
                let count = if matches!(
                    media.result,
                    ForeignLookup::MissingLink
                        | ForeignLookup::UnknownOwnerItem
                        | ForeignLookup::OwnerNamesake
                ) {
                    0
                } else {
                    2 * lookups
                };
                assert_eq!(media.lookups.load(Ordering::SeqCst), count);
                assert_eq!(
                    rows(
                        &fixture.database,
                        "SELECT json_extract(CAST(manifest AS TEXT), '$.decision.media_source.zone.zoneName')
                         FROM provider_active_decisions WHERE child='asset-accepted-page-master'",
                    ),
                    [vec![SqlValue::Text(FOREIGN_ZONE.into())]],
                    "{fault:?} cycle {cycle}",
                );
                expected_records = vec![vec![SqlValue::Text(ACCEPTED_RELATION.into())]];
            }
            if missing_master || retained_link || favorites || foreign.is_some() {
                if missing_master {
                    expected_records = vec![vec![SqlValue::Text(child.clone())]];
                }
                if cycle == 0 || !verified_accepted {
                    assert_eq!(lookups, usize::from(cycle == 0), "{fault:?} cycle {cycle}");
                } else {
                    assert!(lookups <= 1, "{fault:?} cycle {cycle}");
                }
                let (admission, reason, attempts) = if verified_accepted {
                    ("admitted", "", 0)
                } else {
                    ("deferred", "current_lookup_unresolved", 1)
                };
                assert_eq!(
                    rows(
                        &fixture.database,
                        "SELECT admission,reason,attempts FROM provider_active_decisions
                         WHERE child='asset-accepted-page-master'",
                    ),
                    [vec![
                        SqlValue::Text(admission.into()),
                        SqlValue::Text(reason.into()),
                        SqlValue::Integer(attempts)
                    ]],
                    "{fault:?}"
                );
            }
            if mixed_unfiled {
                expected_records.push(vec![SqlValue::Text(REMOVED_LINK.into())]);
                if two_album_removals {
                    expected_records.push(vec![SqlValue::Text(SECOND_REMOVED_LINK.into())]);
                }
                let (admission, attempts) = if verified_accepted {
                    ("admitted", 0)
                } else {
                    ("deferred", 1)
                };
                assert_eq!(lookups, usize::from(cycle == 0 || verified_accepted));
                assert_eq!(
                    rows(
                        &fixture.database,
                        "SELECT d.admission,d.attempts,json_extract(p.value,'$.kind')
                         FROM provider_active_decisions d
                         JOIN provider_active_generations g ON g.id=d.generation
                         JOIN json_each(CAST(g.specification AS TEXT),'$.profile.passes') p
                           ON json_extract(p.value,'$.key')=d.pass_key
                         WHERE d.child='asset-accepted-page-master'",
                    ),
                    [vec![
                        SqlValue::Text(admission.into()),
                        SqlValue::Integer(attempts),
                        SqlValue::Text("Unfiled".into())
                    ]]
                );
            } else if named_album && !retained_link && foreign.is_none() {
                expected_records.push(vec![SqlValue::Text(ACCEPTED_RELATION.into())]);
            }
            if mixed_albums {
                expected_records.extend(
                    [
                        UNRESOLVED_MASTER,
                        "asset-unresolved-master",
                        UNRESOLVED_RELATION,
                    ]
                    .map(|name| vec![SqlValue::Text(name.into())]),
                );
                assert_eq!(lookups, if cycle == 0 { 2 } else { 1 });
                assert_eq!(
                    rows(
                        &fixture.database,
                        "SELECT admission,reason,attempts FROM provider_active_decisions
                         WHERE child='asset-unresolved-master'",
                    ),
                    [vec![
                        SqlValue::Text("deferred".into()),
                        SqlValue::Text("current_lookup_unresolved".into()),
                        SqlValue::Integer(1)
                    ]]
                );
            }
            if overlap {
                expected_records.push(vec![SqlValue::Text(UNRESOLVED_RELATION.into())]);
                assert_eq!(lookups, if cycle == 0 || second_copy { 2 } else { 1 });
                let checksum = SqlValue::Text(format!("{:x}", Sha256::digest(bytes)));
                let mut expected = Vec::new();
                for album in [ACCEPTED_ALBUM, UNRESOLVED_ALBUM] {
                    let file = fixture.media.join(album).join("accepted.JPG");
                    if album == ACCEPTED_ALBUM || second_copy {
                        expected.push(vec![
                            SqlValue::Text(file.to_str().unwrap().into()),
                            checksum.clone(),
                        ]);
                    } else {
                        assert!(!tokio::fs::try_exists(&file).await.unwrap());
                    }
                }
                assert_eq!(
                    rows(
                        &fixture.database,
                        "SELECT compat_path,local_checksum FROM provider_active_destinations
                         WHERE library='PrimarySync' AND child='asset-accepted-page-master'
                           AND admitted=1 AND verified_media=1 ORDER BY compat_path",
                    ),
                    expected,
                    "{fault:?}; decisions: {:?}",
                    rows(
                        &fixture.database,
                        "SELECT admission,reason FROM provider_active_decisions
                         WHERE child='asset-accepted-page-master' ORDER BY pass_key",
                    ),
                );
                for row in expected {
                    let [SqlValue::Text(path), _] = row.as_slice() else {
                        panic!("Expected a destination path and checksum");
                    };
                    assert_eq!(tokio::fs::read(path).await.unwrap(), *bytes);
                }
            }
            if let Some((_, lookup, same_folder)) = favorite_mix {
                let favorite = lookup == FavoriteLookup::Current;
                if lookup == FavoriteLookup::Malformed {
                    assert_eq!(
                        rows(
                            &fixture.database,
                            "SELECT reason FROM provider_active_decisions WHERE child='asset-accepted-page-master' AND admission='deferred'"
                        ),
                        [vec![SqlValue::Text("current_lookup_unresolved".into())]]
                    );
                }
                let destinations = rows(
                    &fixture.database,
                    "SELECT DISTINCT compat_path FROM provider_active_destinations WHERE child='asset-accepted-page-master' AND admitted=1 AND verified_media=1 ORDER BY compat_path",
                );
                assert_eq!(
                    destinations.len(),
                    1 + usize::from(favorite),
                    "{fault:?}: {result:?}"
                );
                for row in destinations {
                    let [SqlValue::Text(path)] = row.as_slice() else {
                        panic!("missing verified path")
                    };
                    assert_eq!(std::fs::read(path).unwrap(), *bytes);
                }
                assert!(
                    fixture
                        .media
                        .join(if same_folder { FAVORITES } else { "unfiled" })
                        .join("accepted.JPG")
                        .is_file()
                );
                assert_eq!(
                    fixture.media.join(FAVORITES).join("accepted.JPG").is_file(),
                    favorite || same_folder
                );
            }
            assert_eq!(
                rows(
                    &fixture.database,
                    "SELECT record_name FROM provider_shadow_records r
                     JOIN provider_shadow_pages p ON p.id=r.page_id
                     WHERE p.successor='accepted-page' ORDER BY r.ordinal"
                ),
                expected_records
            );
            let accepted = installed.iter().find(|row| {
                row.library.as_ref()
                    == if foreign.is_some() {
                        FOREIGN_ZONE
                    } else {
                        ZONE
                    }
                    && row.id.as_ref() == child
            });
            if verified_accepted {
                let accepted = accepted
                    .expect("the accepted page's download must survive continuation rejection");
                if mixed_unfiled {
                    let path = accepted.local_path.as_ref().unwrap();
                    for album in [ACCEPTED_ALBUM, UNRESOLVED_ALBUM] {
                        assert!(!path.starts_with(fixture.media.join(album)));
                    }
                } else if (named_album && !overlap) || favorites {
                    let folder = if favorites { FAVORITES } else { ACCEPTED_ALBUM };
                    assert_eq!(
                        accepted.local_path.as_ref().unwrap().parent(),
                        Some(fixture.media.join(folder).as_path()),
                    );
                }
                assert_eq!(
                    accepted.local_checksum.as_deref(),
                    Some(format!("{:x}", Sha256::digest(bytes)).as_str())
                );
                assert_eq!(
                    tokio::fs::read(accepted.local_path.as_ref().unwrap())
                        .await
                        .unwrap(),
                    *bytes
                );
            } else {
                assert!(accepted.is_none());
                if favorites || foreign.is_some() {
                    let folder = if favorites { FAVORITES } else { ACCEPTED_ALBUM };
                    assert!(!fixture.media.join(folder).join("accepted.JPG").exists());
                }
            }
            if named_album {
                let links = rows(
                    &fixture.database,
                    "SELECT album_name FROM asset_albums
                     WHERE library IN ('PrimarySync','SharedSync-MEDIA')
                       AND asset_id='asset-accepted-page-master'
                     ORDER BY album_name",
                );
                let mut expected = Vec::new();
                if verified_accepted && !mixed_unfiled {
                    expected.push(vec![SqlValue::Text(ACCEPTED_ALBUM.into())]);
                }
                if second_copy {
                    expected.push(vec![SqlValue::Text(UNRESOLVED_ALBUM.into())]);
                }
                assert_eq!(links, expected);
            } else {
                assert_eq!(provider.membership_lookups.load(Ordering::SeqCst), 0);
            }
            let coverage = if foreign.is_some() {
                "accepted_foreign_album_replay"
            } else if favorite_mix.is_some() {
                "accepted_favorites_unfiled_replay"
            } else if favorites {
                "accepted_favorites_replay"
            } else if named_album {
                "accepted_named_album_replay"
            } else {
                "accepted_unfiled_replay"
            };
            assert_eq!(
                rows(
                    &fixture.database,
                    "SELECT json_extract(CAST(specification AS TEXT),'$.profile.coverage'),
                         sealed,checkpoint_ready FROM provider_active_generations
                     WHERE json_extract(CAST(specification AS TEXT),'$.profile.coverage') IN
                          ('accepted_unfiled_replay', 'accepted_named_album_replay',
                           'accepted_favorites_replay', 'accepted_favorites_unfiled_replay', 'accepted_foreign_album_replay')"
                ),
                [vec![
                    SqlValue::Text(coverage.into()),
                    SqlValue::Integer(0),
                    SqlValue::Integer(0)
                ]]
            );
        }
        assert_eq!(
            result.stats.downloaded,
            usize::from(!session_expired && cycle == 0)
                + usize::from(verified_accepted && cycle == 0)
                + usize::from(second_copy && cycle == 0)
        );
        fixture.assert_preserved();
    }
    server.verify().await;
}

#[tokio::test]
async fn rejected_config_drift_cursor_promotes_fresh_inventory_atomically() {
    use crate::sync_cycle::pending_zone_token_key;

    for fail_promotion in [false, true] {
        let fixture = Fixture::new(Trigger::Rebaseline).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let config = make_run_cycle_config();
        let hash = download::compute_config_hash(&config);
        let staged_key = pending_zone_token_key(&hash, ZONE);
        let db = open_owned_cycle_db(&fixture.database).await;
        seed_active_failed_transfer(&db).await;
        let retry = rows(&fixture.database, ACTIVE_RETRY_QUERY);
        assert_eq!(retry.len(), 1);
        if fail_promotion {
            db.acquire_lock("fail aggregate configuration promotion")
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER fail_config_promotion BEFORE UPDATE ON metadata
                     WHEN NEW.key='enum_config_hash' AND OLD.value<>NEW.value
                     BEGIN SELECT RAISE(ABORT,'configuration promotion interrupted'); END;",
                )
                .unwrap();
        }
        drop(db);

        for cycle in 0..(3 + usize::from(fail_promotion)) {
            let db = open_owned_cycle_db(&fixture.database).await;
            if fail_promotion && cycle == 1 {
                db.acquire_lock("allow aggregate configuration promotion")
                    .unwrap()
                    .execute_batch("DROP TRIGGER fail_config_promotion")
                    .unwrap();
            }
            let cancel = CancellationToken::new();
            let provider = fixture.session(true, Fault::None, cancel.clone());
            let library = make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
            );
            let builder = make_run_cycle_download_config_builder_with_options(
                &fixture.media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: media_without_photo_downloads(),
                    ..Default::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &cancel,
            )
            .await
            .unwrap();
            let rolled_back = fail_promotion && cycle == 0;
            assert_eq!(
                result.db_sync_token_advance_safe, !rolled_back,
                "{result:?}"
            );
            let active_cursor = if rolled_back {
                PRIOR
            } else if cycle == 0 {
                "fresh-rank-anchor"
            } else {
                "validated-old-replay"
            };
            let active_hash = if rolled_back {
                "old-enumeration-generation"
            } else {
                &hash
            };
            for (key, expected) in [
                (CURSOR_KEY, Some(active_cursor)),
                (ENUM_CONFIG_HASH_KEY, Some(active_hash)),
                (
                    PENDING_ENUM_CONFIG_HASH_KEY,
                    rolled_back.then_some(hash.as_str()),
                ),
                (&staged_key, rolled_back.then_some("fresh-rank-anchor")),
                (HOLD_KEY, None),
            ] {
                assert_eq!(
                    db.get_metadata(key).await.unwrap().as_deref(),
                    expected,
                    "{key} in cycle {cycle}: {result:?}"
                );
            }
            let expected_inventory = usize::from(cycle == 0);
            assert_eq!(provider.retained.load(Ordering::SeqCst), expected_inventory);
            assert_eq!(
                provider.rank_starts.load(Ordering::SeqCst),
                expected_inventory
            );
            if cycle > 0 {
                assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            }
            assert_eq!(rows(&fixture.database, ACTIVE_RETRY_QUERY), retry);
            assert_eq!(result.stats.downloaded, 0);
            fixture.assert_preserved();
        }
    }
}

#[tokio::test]
async fn rejected_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::RejectedContinuation,
        "icloud_changes_continuation_rejected",
    )
    .await;
}

#[tokio::test]
async fn wrong_owner_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::WrongOwnerOnContinuation,
        "icloud_changes_zone_owner_mismatch",
    )
    .await;
}

#[tokio::test]
async fn zone_name_mismatch_cannot_promote_configuration_inventory() {
    for fault in [
        Fault::WrongZoneOnContinuation,
        Fault::MissingZoneNameOnContinuation,
    ] {
        assert_replay_refusal_holds_configuration(fault, "icloud_changes_zone_name_mismatch").await;
    }
}

#[tokio::test]
async fn invalid_zone_identity_cannot_promote_configuration_inventory() {
    for fault in [
        Fault::MissingZoneIdOnContinuation,
        Fault::NullZoneEntryOnContinuation,
    ] {
        assert_replay_refusal_holds_configuration(fault, "icloud_changes_invalid_zone_identity")
            .await;
    }
}

#[tokio::test]
async fn empty_zones_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::EmptyZonesOnContinuation,
        "icloud_changes_empty_zone_results",
    )
    .await;
}

#[tokio::test]
async fn multiple_zones_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::MultipleZonesOnContinuation,
        "icloud_changes_multiple_zone_results",
    )
    .await;
}

#[tokio::test]
async fn invalid_zones_field_cannot_promote_configuration_inventory() {
    for fault in [
        Fault::NullZonesOnContinuation,
        Fault::NullResponseOnContinuation,
    ] {
        assert_replay_refusal_holds_configuration(fault, "icloud_changes_invalid_zones_field")
            .await;
    }
}

#[tokio::test]
async fn repeated_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::RepeatedContinuation,
        "icloud_changes_repeated_continuation",
    )
    .await;
}

#[tokio::test]
async fn record_error_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::RecordErrorOnContinuation,
        "icloud_changes_record_error",
    )
    .await;
}

#[tokio::test]
async fn foreign_record_zone_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::ForeignRecordZoneOnContinuation,
        "icloud_changes_record_scope_mismatch",
    )
    .await;
}

#[tokio::test]
async fn invalid_records_on_continuation_cannot_promote_configuration_inventory() {
    for fault in [
        Fault::MissingRecordsOnContinuation,
        Fault::NonObjectRecordOnContinuation,
        Fault::BlankRecordNameOnContinuation,
    ] {
        assert_replay_refusal_holds_configuration(fault, "icloud_changes_invalid_records").await;
    }
}

#[tokio::test]
async fn invalid_continuation_flag_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::NonBooleanContinuationFlag,
        "icloud_changes_invalid_continuation_flag",
    )
    .await;
}

#[tokio::test]
async fn invalid_reason_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::InvalidReasonOnContinuation,
        "icloud_changes_invalid_reason",
    )
    .await;
}

#[tokio::test]
async fn missing_zone_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::ZoneNotFoundOnContinuation,
        "icloud_changes_zone_not_found",
    )
    .await;
}

#[tokio::test]
async fn retry_later_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::RetryLaterOnContinuation,
        "icloud_changes_retry_later",
    )
    .await;
}

#[tokio::test]
async fn server_error_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::ServerErrorOnContinuation,
        "icloud_changes_server_internal_error",
    )
    .await;
}

#[tokio::test]
async fn unrecognized_zone_error_on_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::UnrecognizedZoneErrorOnContinuation,
        "icloud_changes_unrecognized_zone_error",
    )
    .await;
}

#[tokio::test]
async fn throttled_continuation_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::ThrottledOnContinuation,
        "icloud_changes_zone_throttled",
    )
    .await;
}

#[tokio::test]
async fn throttled_first_replay_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::ThrottledOnFirstReplay,
        "icloud_changes_zone_throttled",
    )
    .await;
}

#[tokio::test]
async fn retry_later_on_first_replay_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::RetryLaterOnFirstReplay,
        "icloud_changes_retry_later",
    )
    .await;
}

#[tokio::test]
async fn server_error_on_first_replay_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::ServerErrorOnFirstReplay,
        "icloud_changes_server_internal_error",
    )
    .await;
}

#[tokio::test]
async fn missing_zone_on_first_replay_cannot_promote_configuration_inventory() {
    assert_replay_refusal_holds_configuration(
        Fault::ZoneNotFoundOnFirstReplay,
        "icloud_changes_zone_not_found",
    )
    .await;
}

async fn assert_replay_refusal_holds_configuration(fault: Fault, expected_reason: &str) {
    let fixture = Fixture::new(Trigger::Rebaseline).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let db = open_owned_cycle_db(&fixture.database).await;
    let config = make_run_cycle_config();
    let hash = download::compute_config_hash(&config);
    let cancel = CancellationToken::new();
    let provider = fixture.session(false, fault, cancel.clone());
    let library = make_run_cycle_library_state_with_album(
        ZONE,
        CURSOR_KEY,
        make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
    );
    let builder = make_run_cycle_download_config_builder_with_options(
        &fixture.media,
        db.clone(),
        RunCycleDownloadConfigOptions {
            media: media_without_photo_downloads(),
            ..Default::default()
        },
    );
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &cancel,
    )
    .await
    .unwrap();
    assert!(!result.can_advance_database_checkpoint(), "{result:?}");
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(expected_reason)
    );
    assert_eq!(provider.rank_starts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.retained.load(Ordering::SeqCst), 1);
    let staged = crate::sync_cycle::pending_zone_token_key(&hash, ZONE);
    for (key, expected) in [
        (ENUM_CONFIG_HASH_KEY, Some("old-enumeration-generation")),
        (PENDING_ENUM_CONFIG_HASH_KEY, Some(hash.as_str())),
        (CURSOR_KEY, Some(PRIOR)),
        (&staged, None),
        (HOLD_KEY, None),
    ] {
        assert_eq!(
            db.get_metadata(key).await.unwrap().as_deref(),
            expected,
            "{key}"
        );
    }
    fixture.assert_preserved();
}

#[tokio::test]
async fn retained_checkpoint_expiry_holds_all_trigger_classes_and_quiet_reopen() {
    for trigger in [Trigger::Eligibility, Trigger::Sparse, Trigger::Legacy] {
        let fixture = Fixture::new(trigger).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let config = make_run_cycle_config();
        let old_hash = {
            let db = open_owned_cycle_db(&fixture.database).await;
            db.get_metadata(ENUM_CONFIG_HASH_KEY).await.unwrap()
        };
        let mut receipt = None;
        for cycle in 0..8 {
            let db = open_owned_cycle_db(&fixture.database).await;
            if [3, 5, 6].contains(&cycle) {
                let mut due: Value =
                    serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap())
                        .unwrap();
                due["next_attempt_at"] = json!(0);
                db.set_metadata(HOLD_KEY, &due.to_string()).await.unwrap();
            }
            let cancel = CancellationToken::new();
            let provider = fixture.session(true, Fault::None, cancel.clone());
            let mut library = make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
            );
            library.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(provider.clone()),
                ZONE,
            );
            if cycle > 0 {
                let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
                crate::sync_loop::precheck::include_pending_local_work(
                    &mut precheck,
                    db.as_ref(),
                    Some(&config.metadata),
                    std::slice::from_ref(&library),
                )
                .await;
                assert!(
                    precheck.should_sync_zone(ZONE),
                    "held selected zone must bypass quiet-provider shortcut"
                );
                assert!(!precheck.should_sync_zone("SharedSync-UNSELECTED"));
            }
            let builder = make_run_cycle_download_config_builder_with_options(
                &fixture.media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: media_without_photo_downloads(),
                    ..Default::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &config,
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &cancel,
            )
            .await
            .unwrap();
            assert!(
                !result.db_sync_token_advance_safe,
                "{trigger:?} cycle {cycle}: {result:?}"
            );
            assert_eq!(
                db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
                Some(PRIOR)
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY).await.unwrap(),
                old_hash
            );
            let raw = db
                .get_metadata(HOLD_KEY)
                .await
                .unwrap()
                .unwrap_or_else(|| {
                    panic!(
                        "expired retained replay must create a durable hold: {trigger:?} cycle {cycle}: {result:?}; queries={} retained={}",
                        provider.queries.load(Ordering::SeqCst),
                        provider.retained.load(Ordering::SeqCst),
                    )
                });
            let hold: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(hold["version"], 1);
            assert_eq!(
                hold["enum_config_hash"],
                download::compute_config_hash(&config)
            );
            assert_eq!(
                hold["attempt_count"],
                if cycle < 3 {
                    1
                } else if cycle < 5 {
                    2
                } else if cycle == 5 {
                    3
                } else {
                    4
                }
            );
            assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
            for private in [
                PRIOR,
                "private-owner",
                "private-provider-url",
                "legacy-master",
            ] {
                assert!(!raw.contains(private), "hold must be redacted: {raw}");
            }
            if [0, 3, 5, 6].contains(&cycle) {
                assert!(provider.queries.load(Ordering::SeqCst) > 0);
                assert!(provider.retained.load(Ordering::SeqCst) > 0);
                receipt = Some(raw);
            } else {
                assert_eq!(
                    provider.requests.load(Ordering::SeqCst),
                    0,
                    "held zone must perform no provider operation"
                );
                assert_eq!(
                    provider.queries.load(Ordering::SeqCst),
                    0,
                    "held zone must not repeat rank inventory"
                );
                assert_eq!(
                    provider.inventories.load(Ordering::SeqCst),
                    0,
                    "held zone must not repeat legacy preparation"
                );
                assert_eq!(
                    provider.retained.load(Ordering::SeqCst),
                    0,
                    "reopen must preserve bounded retry deadline"
                );
                assert_eq!(Some(raw), receipt);
            }
            assert_eq!(
                db.sparse_identities(ZONE).await.unwrap().len(),
                fixture.original_sparse.len()
            );
            assert_eq!(
                rows(
                    &fixture.database,
                    "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
                ),
                fixture.original_sparse,
                "sparse original evidence, generation, retries and deletion uncertainty must survive {trigger:?} cycle {cycle}"
            );
            if trigger == Trigger::Legacy {
                let preservation = db.legacy_preservations(ZONE).await.unwrap();
                assert!(
                    preservation.is_empty(),
                    "current filtered selection must not manufacture historical certification"
                );
                let conn = db
                    .acquire_lock("current filtered coverage retains checkpoint rejection")
                    .unwrap();
                let excluded: i64 = conn.query_row("SELECT COUNT(*) FROM provider_active_decisions WHERE outcome='excluded' AND admission='excluded'", [], |row| row.get(0)).unwrap();
                assert!(
                    excluded >= 2,
                    "both current children must reach durable selection"
                );
                let destinations: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM provider_active_destinations WHERE admitted=1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    destinations, 0,
                    "filtered history must not become current copy debt"
                );
                drop(conn);
                assert!(
                    rows(
                        &fixture.database,
                        "SELECT * FROM unattributed_legacy_proofs"
                    )
                    .is_empty()
                );
            }
            if trigger == Trigger::Sparse {
                assert!(
                    db.get_metadata(&state::unresolved_identity_key(ZONE))
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            let summary = db.get_summary().await.unwrap();
            assert_eq!(
                summary.last_recovery_action.as_deref(),
                Some("await_retained_checkpoint_evidence")
            );
            fixture.assert_preserved();
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_hold_valid_old_replay_recovers_then_two_quiet_reopens() {
    // This control repairs a previously rejected OLD cursor. A valid fresh
    // inventory alone never repairs permanent historical-token expiry.
    let fixture = Fixture::new(Trigger::Eligibility).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let config = make_run_cycle_config();
    for cycle in 0..4 {
        let db = open_owned_cycle_db(&fixture.database).await;
        if cycle == 1 {
            let raw = db.get_metadata(HOLD_KEY).await.unwrap().unwrap();
            let mut receipt: Value = serde_json::from_str(&raw).unwrap();
            receipt["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &receipt.to_string())
                .await
                .unwrap();
        }
        let cancel = CancellationToken::new();
        let provider = fixture.session(cycle == 0, Fault::None, cancel.clone());
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(if cycle == 0 {
                PRIOR
            } else {
                "validated-old-replay"
            }),
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(HOLD_KEY).await.unwrap().is_some(),
            cycle == 0
        );
        if cycle > 0 {
            assert!(result.db_sync_token_advance_safe);
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(download::compute_config_hash(&config).as_str())
            );
            assert!(
                db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        if cycle >= 2 {
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
            assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
        }
        fixture.assert_preserved();
    }
}

#[tokio::test]
async fn retained_checkpoint_expiry_controls_never_promote_or_clear_debt() {
    for fault in [
        Fault::Partial,
        Fault::Cancel,
        Fault::HoldWrite,
        Fault::StalePlan,
        Fault::ConcurrentSparse,
        Fault::ConcurrentCursor,
        Fault::ConcurrentConfig,
    ] {
        let trigger = if fault == Fault::HoldWrite {
            Trigger::Eligibility
        } else {
            Trigger::Rebaseline
        };
        let fixture = Fixture::new(trigger).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let db = open_owned_cycle_db(&fixture.database).await;
        if fault == Fault::HoldWrite {
            db.acquire_lock("retained hold write fault").unwrap().execute_batch(
                "CREATE TRIGGER retained_hold_fault BEFORE INSERT ON metadata WHEN NEW.key='retained_checkpoint_hold:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected hold write failure'); END;"
            ).unwrap();
        }
        let cancel = CancellationToken::new();
        let provider = fixture.session(true, fault, cancel.clone());
        let mut library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
        );
        library.plan_is_stale = fault == Fault::StalePlan;
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await;
        assert!(
            result.is_err()
                || result
                    .as_ref()
                    .is_ok_and(|result| !result.db_sync_token_advance_safe),
            "{fault:?}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(if fault == Fault::ConcurrentCursor {
                "independently-advanced-cursor"
            } else {
                PRIOR
            })
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(if fault == Fault::ConcurrentConfig {
                "independently-activated-config"
            } else {
                "old-enumeration-generation"
            })
        );
        if fault == Fault::ConcurrentSparse {
            assert!(provider.injected.load(Ordering::SeqCst));
            assert_eq!(db.sparse_identities(ZONE).await.unwrap().len(), 1);
            assert!(
                db.get_metadata(&state::unresolved_identity_key(ZONE))
                    .await
                    .unwrap()
                    .is_some()
            );
        } else {
            assert!(
                db.get_metadata(HOLD_KEY).await.unwrap().is_none(),
                "incomplete/failed/stale inventories cannot write a completed hold: {fault:?}"
            );
        }
        fixture.assert_preserved();
    }
}

#[derive(Clone)]
struct UnaffectedSession {
    changes: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for UnaffectedSession {
    async fn post(&self, url: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            self.changes.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"zones":[{
                "zoneID":{"zoneName":"SharedSync-CLEAN","ownerRecordName":"_defaultOwner"},
                "syncToken":"clean-zone-successor","moreComing":false,"records":[]
            }]}));
        }
        if url.contains("/internal/records/query/batch?") {
            return Ok(album_count_response(0));
        }
        Ok(json!({"records":[],"syncToken":"clean-inventory-anchor"}))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn retained_checkpoint_held_zone_does_not_block_clean_zone_or_clear_aggregate_hold() {
    let fixture = Fixture::new(Trigger::Sparse).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    {
        let db = open_owned_cycle_db(&fixture.database).await;
        db.set_metadata("sync_token:SharedSync-CLEAN", "clean-zone-before")
            .await
            .unwrap();
    }
    for cycle in 0..3 {
        let db = open_owned_cycle_db(&fixture.database).await;
        let cancel = CancellationToken::new();
        let primary_provider = fixture.session(true, Fault::None, cancel.clone());
        let primary = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_owned_cycle_album(&db, ZONE, "private", Box::new(primary_provider.clone())),
        );
        let clean_changes = Arc::new(AtomicUsize::new(0));
        let clean = make_run_cycle_library_state_with_album(
            "SharedSync-CLEAN",
            "sync_token:SharedSync-CLEAN",
            make_owned_cycle_album(
                &db,
                "SharedSync-CLEAN",
                "private",
                Box::new(UnaffectedSession {
                    changes: clean_changes.clone(),
                }),
            ),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&primary, &clean],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert!(
            !result.db_sync_token_advance_safe,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        assert_eq!(
            db.get_metadata("sync_token:SharedSync-CLEAN")
                .await
                .unwrap()
                .as_deref(),
            Some("clean-zone-successor")
        );
        assert_eq!(
            clean_changes.load(Ordering::SeqCst),
            1,
            "unaffected zone must continue normal polling"
        );
        assert!(db.get_metadata(HOLD_KEY).await.unwrap().is_some());
        assert_eq!(
            db.sparse_identities(ZONE).await.unwrap().len(),
            fixture.original_sparse.len()
        );
        assert_eq!(
            rows(
                &fixture.database,
                "SELECT * FROM unresolved_sparse_identities ORDER BY library,source_record_name"
            ),
            fixture.original_sparse
        );
        assert_eq!(
            db.get_summary()
                .await
                .unwrap()
                .last_recovery_action
                .as_deref(),
            Some("await_retained_checkpoint_evidence")
        );
        if cycle > 0 {
            assert_eq!(primary_provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(primary_provider.retained.load(Ordering::SeqCst), 0);
        }
        fixture.assert_preserved();
    }
}

#[tokio::test]
async fn retained_checkpoint_due_retry_write_failure_prevents_every_provider_request() {
    let fixture = Fixture::new(Trigger::Eligibility).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    for cycle in 0..2 {
        let db = open_owned_cycle_db(&fixture.database).await;
        if cycle == 1 {
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
            db.acquire_lock("due retry reservation failure").unwrap().execute_batch(
                "CREATE TRIGGER retained_retry_fault BEFORE UPDATE ON metadata WHEN NEW.key='retained_checkpoint_hold:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected retry reservation failure'); END;"
            ).unwrap();
        }
        let cancel = CancellationToken::new();
        let provider = fixture.session(true, Fault::None, cancel.clone());
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await;
        assert!(
            result.is_err()
                || result
                    .as_ref()
                    .is_ok_and(|result| !result.db_sync_token_advance_safe)
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(PRIOR)
        );
        if cycle == 1 {
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
            assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
            let hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            assert_eq!(
                hold["next_attempt_at"], 0,
                "failed reservation cannot acknowledge a future retry"
            );
        }
        fixture.assert_preserved();
    }
}

#[derive(Clone)]
struct ArrivingSourceSession {
    base: RetainedSession,
    source: Arc<std::sync::Mutex<Vec<Value>>>,
    arrival: Arc<Vec<Value>>,
    injected: Arc<AtomicBool>,
    delivered: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for ArrivingSourceSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/internal/records/query/batch?") {
            let count = self
                .source
                .lock()
                .unwrap()
                .iter()
                .filter(|record| record["recordType"] == "CPLAsset")
                .count();
            return Ok(album_count_response(count as u64));
        }
        if url.contains("/records/lookup?") {
            // Confirmation must see the same live source as inventory and delta,
            // including records that arrived after the initial rank snapshot.
            let source = self.source.lock().unwrap();
            let records: Vec<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|wanted| {
                    source
                        .iter()
                        .find(|record| record["recordName"] == wanted["recordName"])
                        .cloned()
                        .unwrap_or_else(|| {
                            json!({"recordName":wanted["recordName"],"serverErrorCode":"NOT_FOUND"})
                        })
                })
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/records/query?") {
            self.base.queries.fetch_add(1, Ordering::SeqCst);
            let before = self.source.lock().unwrap().clone();
            // The provider fixture owns this independently specified event.
            // The first rank inventory has already taken its snapshot when the
            // change arrives. Subsequent inventory and retained replay see it.
            if !self.injected.swap(true, Ordering::SeqCst) {
                self.source
                    .lock()
                    .unwrap()
                    .extend(self.arrival.iter().cloned());
            }
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|filter| filter["fieldName"] == "startRank")
                .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records": if offset == 0 {before} else {Vec::new()}, "syncToken":"arrival-rank-anchor"}),
            );
        }
        if url.contains("/changes/zone?")
            && request["zones"][0]["syncToken"] == PRIOR
            && !self.base.reject_prior
        {
            self.base.retained.fetch_add(1, Ordering::SeqCst);
            let records = self.source.lock().unwrap().clone();
            self.delivered.fetch_add(records.len(), Ordering::SeqCst);
            return Ok(json!({"zones":[{
                "zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},
                "syncToken":"validated-old-replay","moreComing":false,"records":records
            }]}));
        }
        self.base.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn retained_checkpoint_provider_change_during_scan_remains_replayable_then_is_durable() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let fixture = Fixture::new_with_owner(Trigger::Eligibility, Some(&owner)).await;
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&fixture.bytes));
    let mut arrival_page = full_album_page_with_download(
        ZONE,
        "arrived-provider-master",
        "unused-provider-anchor",
        "https://p01.icloud-content.com/arrival.jpg",
        fixture.bytes.len() as u64,
        &checksum,
    );
    arrival_page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
    let arrival = Arc::new(arrival_page["records"].as_array().unwrap().clone());
    let source = Arc::new(std::sync::Mutex::new(Vec::new()));
    let injected = Arc::new(AtomicBool::new(false));
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    for cycle in 0..6 {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&fixture.database, &owner)
                .await
                .unwrap(),
        );
        if cycle == 2 || cycle == 3 {
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
        }
        let cancel = CancellationToken::new();
        let base = fixture.session(cycle < 3, Fault::None, cancel.clone());
        let delivered = Arc::new(AtomicUsize::new(0));
        let provider = ArrivingSourceSession {
            base: base.clone(),
            source: source.clone(),
            arrival: arrival.clone(),
            injected: injected.clone(),
            delivered: delivered.clone(),
        };
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com");
        let album = make_captured_cycle_album(ZONE, capture, Box::new(provider));
        let library = make_run_cycle_library_state_with_album(ZONE, CURSOR_KEY, album);
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert!(injected.load(Ordering::SeqCst));
        assert_eq!(
            *source.lock().unwrap(),
            *arrival,
            "provider still offers the independently specified change"
        );
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(if cycle < 3 {
                PRIOR
            } else {
                "validated-old-replay"
            }),
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(HOLD_KEY).await.unwrap().is_some(),
            cycle < 3
        );
        if cycle < 3 {
            assert!(!result.db_sync_token_advance_safe);
            assert_eq!(
                delivered.load(Ordering::SeqCst),
                0,
                "fresh rank inventory must not count as retained replay"
            );
        } else {
            assert!(
                result.db_sync_token_advance_safe,
                "cycle {cycle}: {result:?}"
            );
            let captured = rows(
                &fixture.database,
                "SELECT record_name,record_type FROM provider_shadow_records WHERE record_name IN ('arrived-provider-master','asset-arrived-provider-master') ORDER BY record_name",
            );
            assert_eq!(
                captured.len(),
                2,
                "actual retained replay must durably capture both arrived records"
            );
            assert_eq!(
                db.get_master_record_name_for_asset(ZONE, "asset-arrived-provider-master")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("arrived-provider-master"),
                "the known newly arrived identity may be added alongside preserved historical mappings"
            );
            if cycle == 3 {
                assert_eq!(delivered.load(Ordering::SeqCst), 2);
            }
        }
        if cycle == 1 || cycle >= 4 {
            assert_eq!(base.queries.load(Ordering::SeqCst), 0);
            assert_eq!(base.retained.load(Ordering::SeqCst), 0);
        }
        fixture.assert_preserved();
    }
}

#[tokio::test]
async fn retained_checkpoint_due_interrupted_inventory_consumes_attempt_without_promotion() {
    for fault in [Fault::Partial, Fault::Cancel] {
        let fixture = Fixture::new(Trigger::Eligibility).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        for cycle in 0..3 {
            let db = open_owned_cycle_db(&fixture.database).await;
            if cycle == 1 {
                let mut hold: Value =
                    serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap())
                        .unwrap();
                hold["next_attempt_at"] = json!(0);
                db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
            }
            let cancel = CancellationToken::new();
            let provider = fixture.session(
                true,
                if cycle == 1 { fault } else { Fault::None },
                cancel.clone(),
            );
            let library = make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
            );
            let builder = make_run_cycle_download_config_builder_with_options(
                &fixture.media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: media_without_photo_downloads(),
                    ..Default::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &make_run_cycle_config(),
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &cancel,
            )
            .await;
            assert!(
                result.is_err()
                    || result
                        .as_ref()
                        .is_ok_and(|result| !result.db_sync_token_advance_safe),
                "{fault:?} cycle {cycle}: {result:?}"
            );
            assert_eq!(
                db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
                Some(PRIOR)
            );
            assert_eq!(
                db.get_metadata(ENUM_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("old-enumeration-generation")
            );
            let hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            assert_eq!(
                hold["attempt_count"],
                if cycle == 0 { 1 } else { 2 },
                "reservation must survive {fault:?}"
            );
            assert!(hold["next_attempt_at"].as_i64().unwrap() > chrono::Utc::now().timestamp());
            if cycle == 1 {
                assert!(provider.queries.load(Ordering::SeqCst) > 0);
            }
            if cycle == 2 {
                assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
                assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
            }
            fixture.assert_preserved();
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_malformed_receipt_allows_current_work_without_forgetting_hold() {
    for invalid in [0, 1, 2] {
        let fixture = Fixture::new(Trigger::Eligibility).await;
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        for cycle in 0..2 {
            let db = open_owned_cycle_db(&fixture.database).await;
            let mut invalid_raw = None;
            if cycle == 1 {
                let valid = db.get_metadata(HOLD_KEY).await.unwrap().unwrap();
                let mut value: Value = serde_json::from_str(&valid).unwrap();
                let raw = match invalid {
                    0 => "{invalid".to_owned(),
                    1 => {
                        value["version"] = json!(2);
                        value.to_string()
                    }
                    _ => {
                        value["unrecognized_authority"] = json!(true);
                        value.to_string()
                    }
                };
                db.set_metadata(HOLD_KEY, &raw).await.unwrap();
                invalid_raw = Some(raw);
            }
            let cancel = CancellationToken::new();
            let provider = fixture.session(true, Fault::None, cancel.clone());
            let library = make_run_cycle_library_state_with_album(
                ZONE,
                CURSOR_KEY,
                make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
            );
            let builder = make_run_cycle_download_config_builder_with_options(
                &fixture.media,
                db.clone(),
                RunCycleDownloadConfigOptions {
                    media: media_without_photo_downloads(),
                    ..Default::default()
                },
            );
            let result = run_cycle(
                &[&library],
                &make_run_cycle_config(),
                Some(db.as_ref()),
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &cancel,
            )
            .await
            .unwrap();
            assert!(!result.db_sync_token_advance_safe, "{result:?}");
            assert_eq!(
                db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
                Some(PRIOR)
            );
            if cycle == 1 {
                assert_eq!(
                    db.get_metadata(HOLD_KEY).await.unwrap(),
                    invalid_raw,
                    "unsupported durable evidence must remain reviewable"
                );
                assert!(provider.queries.load(Ordering::SeqCst) > 0);
                assert_eq!(provider.inventories.load(Ordering::SeqCst), 0);
                assert!(provider.retained.load(Ordering::SeqCst) > 0);
            }
            fixture.assert_preserved();
        }
    }
}

#[tokio::test]
async fn retained_checkpoint_previously_exhausted_recovers_with_current_retry_intact() {
    let fixture = Fixture::new(Trigger::Eligibility).await;
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut failed_history = None;
    for cycle in 0..6 {
        let db = open_owned_cycle_db(&fixture.database).await;
        if cycle == 1 {
            // A receipt from an older kei build already spent its last retry.
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            hold["attempt_count"] = json!(3);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
            // This is active failed work, not an archived or excluded row.
            seed_active_failed_transfer(&db).await;
            failed_history = Some(rows(&fixture.database, ACTIVE_RETRY_QUERY));
            assert_eq!(failed_history.as_ref().unwrap().len(), 1);
        }
        if cycle == 3 {
            let mut hold: Value =
                serde_json::from_str(&db.get_metadata(HOLD_KEY).await.unwrap().unwrap()).unwrap();
            hold["next_attempt_at"] = json!(0);
            db.set_metadata(HOLD_KEY, &hold.to_string()).await.unwrap();
        }
        let cancel = CancellationToken::new();
        let recovered = cycle >= 3;
        let provider = fixture.session(!recovered, Fault::None, cancel.clone());
        let library = make_run_cycle_library_state_with_album(
            ZONE,
            CURSOR_KEY,
            make_owned_cycle_album(&db, ZONE, "private", Box::new(provider.clone())),
        );
        let builder = make_run_cycle_download_config_builder_with_options(
            &fixture.media,
            db.clone(),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                ..Default::default()
            },
        );
        let result = run_cycle(
            &[&library],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(result.db_sync_token_advance_safe, recovered, "{result:?}");
        let expected_cursor = if recovered {
            "validated-old-replay"
        } else {
            PRIOR
        };
        assert_eq!(
            db.get_metadata(CURSOR_KEY).await.unwrap().as_deref(),
            Some(expected_cursor)
        );
        assert_eq!(
            db.get_metadata(HOLD_KEY).await.unwrap().is_none(),
            recovered
        );
        match cycle {
            1 | 2 => assert_eq!(
                provider.requests.load(Ordering::SeqCst),
                0,
                "the old hold must still respect its deadline"
            ),
            3 => assert_eq!(provider.retained.load(Ordering::SeqCst), 1),
            4 | 5 => {
                assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
                assert_eq!(provider.retained.load(Ordering::SeqCst), 0);
                assert_eq!(result.failed_count, 0);
            }
            _ => {}
        }
        if let Some(expected) = &failed_history {
            assert_eq!(
                &rows(&fixture.database, ACTIVE_RETRY_QUERY),
                expected,
                "active retry state changed in cycle {cycle}"
            );
        }
        fixture.assert_preserved();
    }
}
