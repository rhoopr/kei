//! Opt-in live samples use disposable isolated storage; private values are never logged or exported.
use super::{DownloadConfig, DownloadControls};
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct ReadOnlyProbe(reqwest::Client, Arc<AtomicUsize>, usize, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl PhotosSession for ReadOnlyProbe {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let parsed =
            reqwest::Url::parse(url).map_err(|_error| anyhow::anyhow!("invalid read URL"))?;
        anyhow::ensure!(
            [
                "/records/query",
                "/records/lookup",
                "/zones/list",
                "/changes/zone",
                "/internal/records/query/batch"
            ]
            .iter()
            .any(|suffix| parsed.path().ends_with(suffix)),
            "live validation refused a non-read endpoint"
        );
        anyhow::ensure!(
            self.1.fetch_add(1, Ordering::Relaxed) < self.2,
            "sample read budget exhausted"
        );
        let bytes = crate::icloud::photos::session::diagnostic_bytes(
            &self.0,
            url,
            body,
            headers,
            8 * 1024 * 1024,
        )
        .await
        .map_err(|_error| anyhow::anyhow!("private provider read failed"))?;
        self.3.fetch_add(bytes.len(), Ordering::Relaxed);
        serde_json::from_slice(&bytes)
            .map_err(|_error| anyhow::anyhow!("private provider response invalid"))
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
#[ignore = "requires explicit live-session and isolated-download authorization"]
async fn existing_session_primary_and_shared_samples_download_once_then_reuse_after_reopen()
-> Result<(), &'static str> {
    if std::env::var("KEI_SESSION_MEDIA_PROBE").as_deref() != Ok("1") {
        return Err("explicit isolated media opt-in missing");
    }
    let (session, service_root) =
        crate::auth::live_zone_cursor_test::existing_session_for_validation().await?;
    let probe = ReadOnlyProbe(
        session.http_client().clone(),
        Arc::new(AtomicUsize::new(0)),
        24,
        Arc::new(AtomicUsize::new(0)),
    );
    let mut samples = Vec::new();
    for database in ["private", "shared"] {
        let endpoint =
            format!("{service_root}/database/1/com.apple.photos.cloud/production/{database}");
        let listing = probe
            .post(
                &format!("{endpoint}/zones/list"),
                "{}".into(),
                &[("Content-type", "text/plain")],
            )
            .await
            .map_err(|_error| "Photos discovery read failed")?;
        let zones = listing
            .get("zones")
            .and_then(Value::as_array)
            .ok_or("Photos zones missing")?;
        for zone in zones {
            if zone.get("deleted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let identity = zone.get("zoneID").ok_or("Photos scope missing")?;
            let name = identity
                .get("zoneName")
                .and_then(Value::as_str)
                .ok_or("Photos scope invalid")?;
            let category = if name == "PrimarySync" {
                "primary"
            } else if name.starts_with("SharedSync-") {
                "shared_photos"
            } else {
                continue;
            };
            if samples
                .iter()
                .any(|(existing, _, _, _): &(&str, String, String, Value)| *existing == category)
            {
                continue;
            }
            samples.push((
                category,
                database.to_owned(),
                endpoint.clone(),
                identity.clone(),
            ));
        }
    }
    if samples.len() != 2 {
        return Err("both Primary and Shared Photos scopes are required");
    }
    for (category, database, endpoint, identity) in samples {
        let params = Arc::new(std::collections::HashMap::from([
            ("remapEnums".to_owned(), Value::Bool(true)),
            ("getCurrentSyncToken".to_owned(), Value::Bool(true)),
        ]));
        let zone = Arc::new(identity);
        let library = PhotoLibrary::new(
            endpoint.clone(),
            Arc::clone(&params),
            Box::new(probe.clone()),
            Arc::clone(&zone),
            database,
            crate::retry::RetryConfig::default(),
        )
        .await
        .map_err(|_error| "selected Photos library could not be initialized")?;
        // Small asset pages and a bounded request budget. The selected rendition
        // sizes advertised by the provider must total at most 32 MiB.
        let album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params,
                service_endpoint: endpoint.into(),
                name: "".into(),
                list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter: None,
                page_size: 3,
                zone_id: zone,
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(probe.clone()),
        );
        let candidates = album
            .photos(Some(3))
            .await
            .map_err(|_error| "bounded Photos sample read failed")?;
        let asset = candidates
            .into_iter()
            .find(|asset| {
                !asset.versions().is_empty()
                    && asset
                        .versions()
                        .iter()
                        .try_fold(0_u64, |sum, (_, version)| sum.checked_add(version.size))
                        .is_some_and(|size| size > 0 && size <= 32 * 1024 * 1024)
            })
            .ok_or("no bounded downloadable media sample available")?;
        let isolated = tempfile::tempdir().map_err(|_error| "isolated sample directory failed")?;
        let directory = isolated.path().join("media");
        let state = isolated.path().join("state.sqlite");
        let mut snapshot = Vec::new();
        let mut transferred = 0;
        for cycle in 0..3 {
            let db = Arc::new(
                SqliteStateDb::open(&state)
                    .await
                    .map_err(|_error| "isolated sample state failed")?,
            );
            let config = Arc::new(DownloadConfig {
                directory: directory.clone().into(),
                folder_structure: "none".into(),
                library: library.zone_name().into(),
                state_db: Some(db.clone()),
                metadata: crate::config::MetadataConfig::default(),
                ..DownloadConfig::test_default()
            });
            let result = super::pipeline::stream_and_download_from_stream(
                session.download_client(),
                futures_util::stream::iter(vec![Ok::<_, anyhow::Error>(asset.clone())]),
                &config,
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                super::pipeline::StreamRuntime::new(None, None),
            )
            .await
            .map_err(|_error| "isolated sample pipeline failed")?;
            if !result.failed.is_empty()
                || result.state_write_failures != 0
                || result.enumeration_errors != 0
            {
                return Err("isolated sample did not complete");
            }
            if cycle == 0 {
                if result.downloaded == 0 {
                    return Err("live sample did not download selected media");
                }
                transferred = result.downloaded;
                let rows = db
                    .get_downloaded_page(0, 100)
                    .await
                    .map_err(|_error| "isolated sample receipt read failed")?;
                for row in rows {
                    let path = row.local_path.ok_or("live receipt has no path")?;
                    let sha = super::file::compute_sha256(&path)
                        .await
                        .map_err(|_error| "live sample hash failed")?;
                    let mtime = std::fs::metadata(&path)
                        .and_then(|value| value.modified())
                        .map_err(|_error| "live sample timestamp read failed")?;
                    snapshot.push((path, sha, mtime));
                }
            } else {
                if result.downloaded != 0
                    || result.bytes_downloaded != 0
                    || result.skip_summary.on_disk != transferred
                {
                    return Err("live sample was not reused completely after reopen");
                }
                for (path, sha, mtime) in &snapshot {
                    if super::file::compute_sha256(path)
                        .await
                        .map_err(|_error| "reopened sample hash failed")?
                        != *sha
                        || std::fs::metadata(path)
                            .and_then(|value| value.modified())
                            .map_err(|_error| "reopened timestamp read failed")?
                            != *mtime
                    {
                        return Err("existing live sample changed during reuse");
                    }
                }
            }
            drop(config);
            drop(db);
        }
        eprintln!(
            "scope={category} sample_assets=1 downloaded_renditions={transferred} reopened_reuse_cycles=2 existing_bytes_and_mtime_unchanged=true"
        );
    }
    eprintln!(
        "isolated_sample_only=true full_live_baseline_proven=false provider_writes=0 reauthentication_attempted=false"
    );
    Ok(())
}

const CYCLE_TRANSFER_CAP: usize = 32 * 1024 * 1024;
const LEGACY_PROBE_TOKEN: &str = "synthetic-pre-v27-token-must-never-be-sent";

type TransferScope = (std::path::PathBuf, Arc<AtomicUsize>);
static TRANSFER_SCOPES: std::sync::Mutex<Vec<TransferScope>> = std::sync::Mutex::new(Vec::new());

struct TransferBudget(TransferScope);
impl TransferBudget {
    fn new(root: std::path::PathBuf) -> Result<Self, &'static str> {
        let scope = (root, Arc::new(AtomicUsize::new(0)));
        TRANSFER_SCOPES
            .lock()
            .map_err(|_error| "test transfer budget unavailable")?
            .push(scope.clone());
        Ok(Self(scope))
    }
    fn used(&self) -> usize {
        self.0.1.load(Ordering::Relaxed)
    }
}
impl Drop for TransferBudget {
    fn drop(&mut self) {
        if let Ok(mut scopes) = TRANSFER_SCOPES.lock() {
            scopes.retain(|scope| !Arc::ptr_eq(&scope.1, &self.0.1));
        }
    }
}

/// Test-only per-disposable-root gate, before writes/publication. Other tests
/// and production builds are unaffected. The current network chunk may already
/// have arrived, but bytes beyond the budget are never written or published.
pub(crate) fn admit_transfer_chunk(path: &std::path::Path, bytes: usize) -> std::io::Result<()> {
    let scopes = TRANSFER_SCOPES
        .lock()
        .map_err(|_error| std::io::Error::other("test transfer budget unavailable"))?;
    for (_, used) in scopes.iter().filter(|(root, _)| path.starts_with(root)) {
        used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
            total
                .checked_add(bytes)
                .filter(|total| *total <= CYCLE_TRANSFER_CAP)
        })
        .map_err(|_error| std::io::Error::other("isolated live transfer budget exceeded"))?;
    }
    Ok(())
}

struct ResourceEvidence {
    checksum: String,
    size: u64,
    field: &'static str,
    fingerprint: Option<Value>,
    file_type: Option<String>,
}

#[derive(Default)]
struct CycleProbeState {
    inventory_calls: AtomicUsize,
    changes_calls: AtomicUsize,
    last_token: std::sync::Mutex<Option<String>>,
    // Once selected, only that real collection can be enumerated.
    collection: std::sync::Mutex<Option<(String, Option<Value>)>>,
    bounds: std::sync::Mutex<SampleBounds>,
    evidence: std::sync::Mutex<Vec<ResourceEvidence>>,
    field_presence: std::sync::Mutex<std::collections::BTreeMap<&'static str, usize>>,
}
#[derive(Clone, Default)]
struct SampleBounds {
    children: std::collections::HashSet<String>,
    masters: std::collections::HashSet<String>,
    // Shared descriptors can occur on both halves of a single photo pair.
    // Deduplication here bounds advertised bytes only; it never merges identities.
    media: std::collections::HashMap<(String, u64), u64>,
}
impl CycleProbeState {
    fn check_sample_bounds(&self, records: &[&Value]) -> anyhow::Result<()> {
        let mut stored = self
            .bounds
            .lock()
            .map_err(|_error| anyhow::anyhow!("probe bounds unavailable"))?;
        let mut next = stored.clone();
        for record in records {
            let kind = record.get("recordType").and_then(Value::as_str);
            if matches!(kind, Some("CPLAsset" | "CPLMaster")) {
                let name = record
                    .get("recordName")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("sample identity missing"))?;
                if kind == Some("CPLAsset") {
                    next.children.insert(name.to_owned());
                } else {
                    next.masters.insert(name.to_owned());
                }
            }
            if let Some(fields) = record.get("fields").and_then(Value::as_object) {
                for field in crate::icloud::photos::queries::DESIRED_KEYS
                    .iter()
                    .filter(|field| field.ends_with("Res"))
                {
                    let Some(value) = fields.get(*field).and_then(|value| value.get("value"))
                    else {
                        continue;
                    };
                    if let (Some(signature), Some(size)) = (
                        value.get("fileChecksum").and_then(Value::as_str),
                        value.get("size").and_then(Value::as_u64),
                    ) {
                        next.media.insert((signature.to_owned(), size), size);
                    }
                }
            }
        }
        let total = next
            .media
            .values()
            .try_fold(0_u64, |sum, size| sum.checked_add(*size));
        let valid = next.children.len() <= 3
            && next.masters.len() <= 3
            && next.media.len() <= 128
            && total.is_some_and(|total| total <= CYCLE_TRANSFER_CAP as u64);
        if !valid {
            eprintln!(
                "sample_bound_refused children={} masters={} media_descriptors={} advertised_media_bytes={} byte_overflow={}",
                next.children.len(),
                next.masters.len(),
                next.media.len(),
                total.unwrap_or(0),
                total.is_none()
            );
            anyhow::bail!("collection changed beyond bounded photo-pair or media budget");
        }
        *stored = next;
        Ok(())
    }
}

#[derive(Clone)]
struct CycleProbe {
    readonly: ReadOnlyProbe,
    state: Arc<CycleProbeState>,
}
#[async_trait::async_trait]
impl PhotosSession for CycleProbe {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(
            !body.contains(LEGACY_PROBE_TOKEN),
            "old token escaped test state"
        );
        let parsed =
            reqwest::Url::parse(url).map_err(|_error| anyhow::anyhow!("invalid provider URL"))?;
        let request: Value = serde_json::from_str(&body)
            .map_err(|_error| anyhow::anyhow!("invalid read request"))?;
        let inventory = parsed.path().ends_with("/records/query")
            && request.pointer("/query/recordType").and_then(Value::as_str)
                != Some("CheckIndexingState");
        if inventory {
            if let Some((record_type, filter)) = self
                .state
                .collection
                .lock()
                .map_err(|_error| anyhow::anyhow!("probe state unavailable"))?
                .as_ref()
            {
                anyhow::ensure!(
                    request.pointer("/query/recordType").and_then(Value::as_str)
                        == Some(record_type.as_str()),
                    "normal cycle requested an unselected inventory"
                );
                let actual: Vec<Value> = request
                    .pointer("/query/filterBy")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|field| {
                        !matches!(
                            field.get("fieldName").and_then(Value::as_str),
                            Some("startRank" | "direction")
                        )
                    })
                    .cloned()
                    .collect();
                let expected = filter
                    .as_ref()
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                anyhow::ensure!(
                    actual == expected,
                    "normal cycle broadened collection selection"
                );
            }
            self.state.inventory_calls.fetch_add(1, Ordering::Relaxed);
        }
        let response = self.readonly.post(url, body, headers).await?;
        let selected = self
            .state
            .collection
            .lock()
            .map_err(|_error| anyhow::anyhow!("probe state unavailable"))?
            .is_some();
        if selected {
            let mut batches: Vec<&Value> = response
                .get("records")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .collect();
            for zone in response
                .get("zones")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                batches.extend(
                    zone.get("records")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                );
            }
            self.state.check_sample_bounds(&batches)?;
            for record in &batches {
                let mut presence = self
                    .state
                    .field_presence
                    .lock()
                    .map_err(|_error| anyhow::anyhow!("probe evidence unavailable"))?;
                if record.get("recordChangeTag").is_some() {
                    *presence.entry("recordChangeTag").or_default() += 1;
                }
                if let Some(fields) = record.get("fields").and_then(Value::as_object) {
                    for field in [
                        "masterRef",
                        "remappedRef",
                        "isSparsePrivateRecord",
                        "linkedShareRecordName",
                        "linkedShareZoneName",
                        "linkedShareZoneOwner",
                    ] {
                        if fields.contains_key(field) {
                            *presence.entry(field).or_default() += 1;
                        }
                    }
                    let mut evidence = self
                        .state
                        .evidence
                        .lock()
                        .map_err(|_error| anyhow::anyhow!("probe evidence unavailable"))?;
                    for &field in crate::icloud::photos::queries::DESIRED_KEYS
                        .iter()
                        .filter(|field| field.ends_with("Res"))
                    {
                        let Some(value) = fields.get(field).and_then(|value| value.get("value"))
                        else {
                            continue;
                        };
                        for field in [
                            "fileChecksum",
                            "referenceChecksum",
                            "wrappingKey",
                            "receipt",
                        ] {
                            if value.get(field).is_some() {
                                *presence.entry(field).or_default() += 1;
                            }
                        }
                        let (Some(checksum), Some(size)) = (
                            value.get("fileChecksum").and_then(Value::as_str),
                            value.get("size").and_then(Value::as_u64),
                        ) else {
                            continue;
                        };
                        if evidence.iter().any(|prior| {
                            prior.checksum == checksum && prior.size == size && prior.field == field
                        }) {
                            continue;
                        }
                        let stem = field
                            .strip_suffix("Res")
                            .ok_or_else(|| anyhow::anyhow!("invalid resource field"))?;
                        anyhow::ensure!(
                            evidence.len() < 128,
                            "bounded resource evidence exhausted"
                        );
                        evidence.push(ResourceEvidence {
                            checksum: checksum.to_owned(),
                            size,
                            field,
                            fingerprint: fields
                                .get(&format!("{stem}Fingerprint"))
                                .and_then(|value| value.get("value"))
                                .cloned(),
                            file_type: fields
                                .get(&format!("{stem}FileType"))
                                .and_then(|value| value.get("value"))
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        });
                    }
                }
            }
        }
        if parsed.path().ends_with("/changes/zone") {
            self.state.changes_calls.fetch_add(1, Ordering::Relaxed);
            let zones = response
                .get("zones")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("zone delta missing"))?;
            anyhow::ensure!(zones.len() == 1, "unexpected delta scope count");
            let token = zones[0]
                .get("syncToken")
                .and_then(Value::as_str)
                .filter(|token| !token.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("delta token missing"))?;
            *self
                .state
                .last_token
                .lock()
                .map_err(|_error| anyhow::anyhow!("probe state unavailable"))? =
                Some(token.to_owned());
        }
        Ok(response)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
#[ignore = "explicit saved-session normal-cycle isolated-download authorization required"]
async fn existing_session_normal_baseline_then_two_reopened_incremental_cycles()
-> Result<(), &'static str> {
    normal_cycle_validation(false).await
}

async fn normal_cycle_validation(digest_probe: bool) -> Result<(), &'static str> {
    use crate::commands::{AlbumPass, AlbumPlan, PassKind, PassScope};
    use crate::icloud::photos::inbox::ShadowCapture;
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig};
    use crate::sync_cycle::{LibraryState, run_cycle};
    if std::env::var("KEI_SESSION_NORMAL_CYCLE_PROBE").as_deref() != Ok("1")
        || std::env::var_os("KEI_REQUEST_DUMP_DIR").is_some()
    {
        return Err("normal-cycle opt-in missing or private request dumping enabled");
    }
    let (session, service_root, owner) =
        crate::auth::live_zone_cursor_test::existing_session_and_owner_for_validation().await?;
    if std::env::var_os("KEI_REQUEST_DUMP_DIR").is_some() {
        return Err("private request dumping is incompatible with this validation");
    }
    let username =
        std::env::var("ICLOUD_USERNAME").map_err(|_error| "validated username unavailable")?;
    let readonly = ReadOnlyProbe(
        session.http_client().clone(),
        Arc::new(AtomicUsize::new(0)),
        96,
        Arc::new(AtomicUsize::new(0)),
    );
    let shared = Arc::new(tokio::sync::RwLock::new(session));
    let mut scopes = Vec::new();
    for database in ["private", "shared"] {
        let endpoint =
            format!("{service_root}/database/1/com.apple.photos.cloud/production/{database}");
        let listing = readonly
            .post(
                &format!("{endpoint}/zones/list"),
                "{}".into(),
                &[("Content-type", "text/plain")],
            )
            .await
            .map_err(|_error| "bounded scope discovery failed")?;
        for zone in listing
            .get("zones")
            .and_then(Value::as_array)
            .ok_or("scope listing unavailable")?
        {
            if zone.get("deleted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let identity = zone.get("zoneID").ok_or("scope identity unavailable")?;
            let name = identity
                .get("zoneName")
                .and_then(Value::as_str)
                .ok_or("scope name unavailable")?;
            let category = if name == "PrimarySync" {
                "primary"
            } else if name.starts_with("SharedSync-") {
                "shared_photos"
            } else {
                continue;
            };
            if !scopes
                .iter()
                .any(|(category_seen, _, _, _): &(&str, String, String, Value)| {
                    *category_seen == category
                })
            {
                scopes.push((
                    category,
                    database.to_owned(),
                    endpoint.clone(),
                    identity.clone(),
                ));
            }
        }
    }
    if scopes.len() != 2 {
        return Err("both Primary and Shared Photos scopes are required");
    }
    for (category, database, endpoint, identity) in scopes {
        let params = Arc::new(std::collections::HashMap::from([
            ("remapEnums".to_owned(), Value::Bool(true)),
            ("getCurrentSyncToken".to_owned(), Value::Bool(true)),
        ]));
        let zone = Arc::new(identity);
        let probe = CycleProbe {
            readonly: readonly.clone(),
            state: Arc::new(CycleProbeState::default()),
        };
        let mut selected = None;
        // Real built-in collections, selected only after a nonzero bounded
        // provider count and a complete preflight, never a fabricated sample.
        for (name, definition) in crate::icloud::photos::smart_folders::smart_folders()
            .into_iter()
            .filter(|(_, definition)| !definition.is_sensitive)
        {
            let album = PhotoAlbum::new(
                PhotoAlbumConfig {
                    params: params.clone(),
                    service_endpoint: endpoint.clone().into(),
                    name: name.into(),
                    list_type: definition.list_type.into(),
                    obj_type: definition.obj_type.into(),
                    query_filter: definition.query_filter.clone(),
                    page_size: 3,
                    zone_id: zone.clone(),
                    retry_config: crate::retry::RetryConfig::default(),
                    container_id: None,
                    cross_zone_sources: Vec::new(),
                },
                Box::new(probe.clone()),
            );
            let count = album
                .len()
                .await
                .map_err(|_error| "bounded collection count unavailable")?;
            if !(1..=3).contains(&count) {
                continue;
            }
            let assets = album
                .photos(None)
                .await
                .map_err(|_error| "bounded collection preflight failed")?;
            let size = assets
                .iter()
                .flat_map(|asset| asset.versions())
                .try_fold(0_u64, |sum, (_, version)| sum.checked_add(version.size));
            if assets.len() != count as usize
                || !size.is_some_and(|size| size > 0 && size <= CYCLE_TRANSFER_CAP as u64)
            {
                continue;
            }
            selected = Some((name, definition, count));
            break;
        }
        let (name, definition, count) = selected
            .ok_or("no genuinely bounded existing nonsensitive collection in this scope")?;
        *probe
            .state
            .collection
            .lock()
            .map_err(|_error| "probe state unavailable")? = Some((
            definition.list_type.to_owned(),
            definition.query_filter.as_deref().cloned(),
        ));
        let isolated =
            tempfile::tempdir().map_err(|_error| "isolated cycle directory unavailable")?;
        let directory = isolated.path().join("media");
        let state_path = isolated.path().join("state.sqlite");
        let budget = TransferBudget::new(directory.clone())?;
        let globals = crate::config::GlobalArgs {
            username: Some(username.clone()),
            domain: Some(crate::types::Domain::Com),
            data_dir: Some(isolated.path().to_string_lossy().into_owned()),
        };
        let mut config = crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            crate::cli::SyncArgs::default(),
            None,
        )
        .map_err(|_error| "isolated cycle configuration invalid")?;
        config.download.directory = directory.clone();
        config.runtime = crate::config::RuntimeConfig {
            dry_run: false,
            only_print_filenames: false,
            refresh_metadata: false,
            repair_capture_timestamps: false,
            repair_truncated: false,
        };
        config.metadata = crate::config::MetadataConfig::default();
        config.filters.recent = None;
        config.filters.skip_created_before = None;
        config.filters.skip_created_after = None;
        config.filters.skip_photos = false;
        config.filters.skip_videos = false;
        config.report.json = None;
        config.notifications.script = None;
        config.filters.selection.albums = crate::selection::AlbumSelector::None;
        config.filters.selection.albums_explicit = true;
        config.filters.selection.smart_folders = crate::selection::SmartFolderSelector::Named {
            included: std::collections::BTreeSet::from([name.to_owned()]),
            excluded: Default::default(),
        };
        config.filters.selection.smart_folders_explicit = true;
        config.filters.selection.unfiled = false;
        let mut snapshot = Vec::new();
        for cycle in 0..3 {
            let db = Arc::new(
                SqliteStateDb::open_owned(&state_path, &owner)
                    .await
                    .map_err(|_error| "isolated owned database failed")?,
            );
            let mut library = PhotoLibrary::new(
                endpoint.clone(),
                params.clone(),
                Box::new(probe.clone()),
                zone.clone(),
                database.clone(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .map_err(|_error| "selected library unavailable")?;
            let capture = ShadowCapture::new(db.clone(), owner.clone(), "com");
            library.shadow_capture = Some(capture.clone());
            let mut album = PhotoAlbum::new(
                PhotoAlbumConfig {
                    params: params.clone(),
                    service_endpoint: endpoint.clone().into(),
                    name: name.into(),
                    list_type: definition.list_type.into(),
                    obj_type: definition.obj_type.into(),
                    query_filter: definition.query_filter.clone(),
                    page_size: 3,
                    zone_id: zone.clone(),
                    retry_config: crate::retry::RetryConfig::default(),
                    container_id: None,
                    cross_zone_sources: Vec::new(),
                },
                Box::new(probe.clone()),
            );
            album.set_shadow_capture(capture, database.clone().into());
            let zone_name = library.zone_name().to_owned();
            let key = crate::sync_cycle::sync_token_key(&zone_name);
            if cycle == 0 {
                db.set_metadata(&format!("sync_token:{zone_name}"), LEGACY_PROBE_TOKEN)
                    .await
                    .map_err(|_error| "legacy token seed failed")?;
            }
            let selected = LibraryState {
                library,
                zone_name,
                sync_token_key: key.clone(),
                cross_zone_libraries: Vec::new(),
                pass_scope: PassScope {
                    include_albums: false,
                    include_smart_folders: true,
                    include_unfiled: false,
                },
                plan: AlbumPlan {
                    passes: vec![AlbumPass {
                        kind: PassKind::SmartFolder,
                        album,
                        exclude_ids: Arc::new(Default::default()),
                    }],
                },
                plan_is_stale: false,
                plan_needs_refresh: false,
            };
            let builder = |sync_mode, exclude_asset_ids, asset_groupings, library| {
                let mut cfg = DownloadConfig::test_default();
                cfg.directory = directory.clone().into();
                cfg.folder_structure = "none".into();
                cfg.folder_structure_smart_folders = "none".into();
                cfg.state_db = Some(db.clone() as Arc<dyn super::DownloadStore>);
                cfg.sync_mode = sync_mode;
                cfg.exclude_asset_ids = exclude_asset_ids;
                cfg.asset_groupings = asset_groupings;
                cfg.library = library;
                cfg.concurrent_downloads = 1;
                if digest_probe {
                    cfg.edited = true;
                    cfg.alternative = true;
                }
                cfg.retry.max_retries = 0;
                Arc::new(cfg)
            };
            let inventory_before = probe.state.inventory_calls.load(Ordering::Relaxed);
            let changes_before = probe.state.changes_calls.load(Ordering::Relaxed);
            let bytes_before = budget.used();
            let result = run_cycle(
                &[&selected],
                &config,
                Some(db.as_ref()),
                false,
                &builder,
                DownloadControls::download_hidden(),
                &shared,
                &CancellationToken::new(),
            )
            .await
            .map_err(|_error| "bounded normal cycle failed; no completion claimed")?;
            if result.failed_count != 0
                || result.session_expired
                || !result.db_sync_token_advance_safe
                || !db
                    .get_pending()
                    .await
                    .map_err(|_error| "pending accounting unavailable")?
                    .is_empty()
                || !db
                    .get_failed()
                    .await
                    .map_err(|_error| "failed accounting unavailable")?
                    .is_empty()
            {
                eprintln!(
                    "normal_cycle_incomplete scope={} cycle={} failures={} session_expired={} token_safe={} downloaded={} pending={} failed={}",
                    category,
                    cycle,
                    result.failed_count,
                    result.session_expired,
                    result.db_sync_token_advance_safe,
                    result.stats.downloaded,
                    db.get_pending()
                        .await
                        .map_err(|_error| "pending accounting unavailable")?
                        .len(),
                    db.get_failed()
                        .await
                        .map_err(|_error| "failed accounting unavailable")?
                        .len()
                );
                eprintln!(
                    "normal_cycle_probe provider_calls={} provider_call_limit={} accepted_response_bytes={} inventory_calls={} changes_calls={}",
                    probe.readonly.1.load(Ordering::Relaxed),
                    probe.readonly.2,
                    probe.readonly.3.load(Ordering::Relaxed),
                    probe.state.inventory_calls.load(Ordering::Relaxed),
                    probe.state.changes_calls.load(Ordering::Relaxed),
                );
                eprintln!(
                    "normal_cycle_checkpoint transfer_failures={} uncertain={} state_write_failures={} enumeration_errors={} enumeration_incomplete={} identity_incomplete={} token_blocked={} reason={}",
                    result.stats.failed,
                    result.stats.skipped.uncertain,
                    result.stats.state_write_failures,
                    result.stats.enumeration_errors,
                    result.stats.enumeration_incomplete,
                    result.stats.identity_incomplete,
                    result.stats.sync_token_blocked,
                    result.stats.sync_token_blocked_reason.unwrap_or("none"),
                );
                return Err("normal cycle retained incomplete work; no completion claimed");
            }
            let (_, ledger_scope, _) = selected.plan.passes[0]
                .album
                .download_scope()
                .map_err(|_error| "selected ledger scope unavailable")?
                .ok_or("selected ledger is not bound to authenticated state")?;
            if db
                .unfinished_selection_debt(owner.clone(), ledger_scope)
                .await
                .map_err(|_error| "selected ledger accounting invalid")?
            {
                return Err("selected ledger retains pending work; no completion claimed");
            }
            if db
                .get_metadata(&format!("sync_token:{}", selected.zone_name))
                .await
                .map_err(|_error| "legacy token retention unavailable")?
                .as_deref()
                != Some(LEGACY_PROBE_TOKEN)
            {
                return Err("normal baseline altered the preserved old token");
            }
            let exact = probe
                .state
                .last_token
                .lock()
                .map_err(|_error| "token observation unavailable")?
                .clone();
            if exact.is_none()
                || db
                    .get_metadata(&key)
                    .await
                    .map_err(|_error| "checkpoint unavailable")?
                    != exact
                || probe.state.changes_calls.load(Ordering::Relaxed) == changes_before
            {
                return Err("normal cycle did not commit the exact observed successor");
            }
            if cycle == 0 {
                if result.stats.downloaded == 0 {
                    return Err("selected normal baseline downloaded no content");
                }
                for row in db
                    .get_downloaded_page(0, 100)
                    .await
                    .map_err(|_error| "receipt read failed")?
                {
                    let path = row.local_path.ok_or("downloaded receipt has no path")?;
                    let hash = super::file::compute_sha256(&path)
                        .await
                        .map_err(|_error| "sample hash failed")?;
                    let modified = std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .map_err(|_error| "sample timestamp unavailable")?;
                    snapshot.push((path, hash, modified));
                }
                if digest_probe {
                    report_v1_digest_evidence(&probe, db.as_ref(), category).await?;
                }
            } else if result.stats.downloaded != 0
                || budget.used() != bytes_before
                || probe.state.inventory_calls.load(Ordering::Relaxed) != inventory_before
            {
                return Err("reopened normal cycle repeated transfer or inventory");
            }
            for (path, hash, modified) in &snapshot {
                if super::file::compute_sha256(path)
                    .await
                    .map_err(|_error| "reopened sample hash failed")?
                    != *hash
                    || std::fs::metadata(path)
                        .and_then(|metadata| metadata.modified())
                        .map_err(|_error| "reopened timestamp unavailable")?
                        != *modified
                {
                    return Err("existing isolated media changed");
                }
            }
        }
        eprintln!(
            "scope={category} selected_assets={count} normal_baseline_and_reopened_incremental_cycles=3 bytes_and_mtime_stable=true"
        );
    }
    Ok(())
}

#[test]
fn normal_live_transfer_budget_rejects_overrun_before_write_and_is_root_scoped() {
    let isolated = tempfile::tempdir().unwrap();
    let root = isolated.path().join("bounded");
    let budget = TransferBudget::new(root.clone()).unwrap();
    assert!(admit_transfer_chunk(&root.join("media.part"), CYCLE_TRANSFER_CAP + 1).is_err());
    assert_eq!(budget.used(), 0);
    admit_transfer_chunk(
        &isolated.path().join("other/media.part"),
        CYCLE_TRANSFER_CAP + 1,
    )
    .unwrap();
    assert_eq!(budget.used(), 0);
    admit_transfer_chunk(&root.join("media.part"), CYCLE_TRANSFER_CAP).unwrap();
    assert!(admit_transfer_chunk(&root.join("companion.part"), 1).is_err());
    assert_eq!(budget.used(), CYCLE_TRANSFER_CAP);
    assert!(!root.exists());
}

// Public candidate algorithm, NOT a production proof rule:
// https://github.com/OpenBubbles/rustpush/blob/master/src/icloud/mmcs.rs prepare_put
// https://github.com/OpenBubbles/rustpush/blob/master/src/icloud/cloudkit.rs prepare_cloudkit_put
// The CloudKit caller uses prefix 0x01. Prefix 0x04 is a distinct keyed v2
// scheme and is deliberately reported as unsupported, not inferred from length.
fn public_v1_signature(bytes: &[u8]) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    [vec![0x01], hash.finalize().to_vec()].concat()
}

#[derive(Default)]
struct SignatureCounts {
    fields: usize,
    string_fields: usize,
    json_types: std::collections::BTreeMap<&'static str, usize>,
    string_lengths: std::collections::BTreeMap<usize, usize>,
    decoded_lengths: std::collections::BTreeMap<usize, usize>,
    v1: usize,
    v2_unsupported: usize,
    other: usize,
    matches: usize,
    mutated_matches: usize,
}
impl SignatureCounts {
    fn observe(&mut self, value: Option<&Value>, expected: &[u8], mutated: &[u8]) {
        self.fields += 1;
        let kind = match value {
            None => "missing",
            Some(Value::Null) => "null",
            Some(Value::String(_)) => "string",
            Some(Value::Number(_)) => "number",
            Some(Value::Bool(_)) => "boolean",
            Some(Value::Array(_)) => "array",
            Some(Value::Object(_)) => "object",
        };
        *self.json_types.entry(kind).or_default() += 1;
        let Some(value) = value.and_then(Value::as_str) else {
            return;
        };
        self.string_fields += 1;
        *self.string_lengths.entry(value.len()).or_default() += 1;
        let Ok(decoded) = data_encoding::BASE64.decode(value.as_bytes()) else {
            self.other += 1;
            return;
        };
        *self.decoded_lengths.entry(decoded.len()).or_default() += 1;
        match decoded.as_slice() {
            [0x01, rest @ ..] if rest.len() == 20 => {
                self.v1 += 1;
                self.matches += usize::from(decoded == expected);
                self.mutated_matches += usize::from(decoded == mutated);
            }
            [0x04, rest @ ..] if rest.len() == 20 => self.v2_unsupported += 1,
            _ => self.other += 1,
        }
    }
    fn report(&self, category: &str, field: &str) {
        eprintln!(
            "scope={category} evidence={field} json_type_counts={:?} encoded_length_counts={:?}",
            self.json_types, self.string_lengths
        );
        eprintln!(
            "scope={category} evidence={field} fields={} strings={} decoded_length_counts={:?} prefix1={} prefix4_unsupported={} other={} whole_file_matches={} mutated_last_byte_matches={}",
            self.fields,
            self.string_fields,
            self.decoded_lengths,
            self.v1,
            self.v2_unsupported,
            self.other,
            self.matches,
            self.mutated_matches
        );
    }
}

async fn report_v1_digest_evidence(
    probe: &CycleProbe,
    db: &SqliteStateDb,
    category: &str,
) -> Result<(), &'static str> {
    let rows = db
        .get_downloaded_page(0, 100)
        .await
        .map_err(|_error| "digest sample receipt read failed")?;
    if rows.is_empty() || rows.len() >= 100 {
        return Err("digest sample receipt bound unavailable");
    }
    let mut checksums = SignatureCounts::default();
    let mut fingerprints = SignatureCounts::default();
    let mut kinds = std::collections::HashSet::new();
    let mut renditions = std::collections::HashSet::new();
    let mut files = 0;
    let mut sampled_bytes = 0_u64;
    for row in rows {
        let path = row.local_path.ok_or("digest sample native path missing")?;
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|_error| "digest sample read failed")?;
        sampled_bytes = sampled_bytes
            .checked_add(bytes.len() as u64)
            .ok_or("digest sample size overflow")?;
        if bytes.is_empty() || sampled_bytes > CYCLE_TRANSFER_CAP as u64 {
            return Err("digest sample exceeds bound");
        }
        let expected = public_v1_signature(&bytes);
        let mut changed = bytes;
        let last = changed.last_mut().ok_or("digest sample empty")?;
        *last ^= 1;
        let mutated = public_v1_signature(&changed);
        if expected == mutated {
            return Err("digest negative control did not change");
        }
        let evidence = probe
            .state
            .evidence
            .lock()
            .map_err(|_error| "digest evidence unavailable")?;
        let candidates: Vec<_> = evidence
            .iter()
            .filter(|e| e.checksum == row.checksum.as_ref() && e.size == row.size_bytes)
            .collect();
        if candidates.is_empty() {
            return Err("downloaded rendition lacks captured provider evidence");
        }
        for candidate in candidates {
            checksums.observe(
                Some(&Value::String(candidate.checksum.clone())),
                &expected,
                &mutated,
            );
            fingerprints.observe(candidate.fingerprint.as_ref(), &expected, &mutated);
            renditions.insert(candidate.field);
            if let Some(kind) = &candidate.file_type {
                kinds.insert(kind.clone());
            }
        }
        files += 1;
    }
    eprintln!(
        "scope={category} known_field_presence_observations={:?}",
        &*probe
            .state
            .field_presence
            .lock()
            .map_err(|_error| "field presence unavailable")?
    );
    checksums.report(category, "fileChecksum");
    fingerprints.report(category, "resFingerprint");
    eprintln!(
        "scope={category} whole_files={files} all_sample_bytes={sampled_bytes} distinct_filetypes={} distinct_renditions={} provider_read_calls={} provider_response_bytes_accepted={}",
        kinds.len(),
        renditions.len(),
        probe.readonly.1.load(Ordering::Relaxed),
        probe.readonly.3.load(Ordering::Relaxed)
    );
    if checksums.mutated_matches != 0 || fingerprints.mutated_matches != 0 {
        return Err("provider digest negative control unexpectedly matched");
    }
    if kinds.len() < 2 || renditions.len() < 2 {
        eprintln!(
            "scope={category} digest_coverage_limited=true more_filetypes_or_renditions_required=true"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "explicit saved-session bounded normal-cycle digest diagnostic authorization required"]
async fn existing_session_normal_cycle_v1_digest_diagnostic() -> Result<(), &'static str> {
    if std::env::var("KEI_SESSION_SIGNATURE_PROBE").as_deref() != Ok("1") {
        return Err("explicit digest diagnostic opt-in missing");
    }
    normal_cycle_validation(true).await
}

#[test]
fn public_v1_candidate_is_whole_file_and_does_not_treat_v2_as_v1() {
    let mut bytes = vec![0x31; 65537];
    let original = public_v1_signature(&bytes);
    bytes[65536] ^= 1;
    let mutated = public_v1_signature(&bytes);
    assert_ne!(original, mutated);
    let mut counts = SignatureCounts::default();
    counts.observe(
        Some(&Value::String(data_encoding::BASE64.encode(&original))),
        &original,
        &mutated,
    );
    let mut v2 = original.clone();
    v2[0] = 4;
    counts.observe(
        Some(&Value::String(data_encoding::BASE64.encode(&v2))),
        &original,
        &mutated,
    );
    assert_eq!(
        (
            counts.v1,
            counts.matches,
            counts.mutated_matches,
            counts.v2_unsupported
        ),
        (1, 1, 0, 1)
    );
}

#[test]
fn live_sample_bounds_accept_three_pairs_and_reject_fourth_child_without_poisoning_indexing() {
    let state = CycleProbeState::default();
    let mut records = Vec::new();
    for index in 0..3 {
        for kind in ["CPLAsset", "CPLMaster"] {
            records.push(serde_json::json!({
                "recordType":kind,"recordName":format!("{kind}-{index}"),
                "fields":{
                    "resOriginalRes":{"value":{"fileChecksum":format!("public-signature-{index}"),"size":1}},
                    "resComputeStateData":{"value":{"fileChecksum":"compute-not-media","size":999999999}}
                }
            }));
        }
    }
    state
        .check_sample_bounds(&records.iter().collect::<Vec<_>>())
        .unwrap();
    let fourth = serde_json::json!({"recordType":"CPLAsset","recordName":"fourth"});
    assert!(state.check_sample_bounds(&[&fourth]).is_err());
    state.check_sample_bounds(&[&serde_json::json!({"recordName":"index-state","fields":{"state":{"value":"FINISHED"}}})]).unwrap();
    let retained = state.bounds.lock().unwrap();
    assert_eq!(
        (
            retained.children.len(),
            retained.masters.len(),
            retained.media.values().sum::<u64>()
        ),
        (3, 3, 3)
    );
}

#[tokio::test]
#[ignore = "explicit saved-session bounded rendition-download signature diagnostic authorization required"]
async fn existing_session_bounded_primary_shared_rendition_signature_samples()
-> Result<(), &'static str> {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig};
    use crate::types::{AssetVersionSize, PhotoResolution};
    if std::env::var("KEI_SESSION_SIGNATURE_PROBE").as_deref() != Ok("1")
        || std::env::var("KEI_SESSION_MEDIA_PROBE").as_deref() != Ok("1")
        || std::env::var_os("KEI_REQUEST_DUMP_DIR").is_some()
    {
        return Err(
            "explicit isolated signature sample opt-ins missing or request dumping enabled",
        );
    }
    let (session, service_root, owner) =
        crate::auth::live_zone_cursor_test::existing_session_and_owner_for_validation().await?;
    if std::env::var_os("KEI_REQUEST_DUMP_DIR").is_some() {
        return Err("request dumping incompatible with signature samples");
    }
    let readonly = ReadOnlyProbe(
        session.http_client().clone(),
        Arc::new(AtomicUsize::new(0)),
        64,
        Arc::new(AtomicUsize::new(0)),
    );
    let mut scopes = Vec::new();
    for database in ["private", "shared"] {
        let endpoint =
            format!("{service_root}/database/1/com.apple.photos.cloud/production/{database}");
        let listing = readonly
            .post(
                &format!("{endpoint}/zones/list"),
                "{}".into(),
                &[("Content-type", "text/plain")],
            )
            .await
            .map_err(|_error| "sample scope discovery failed")?;
        for zone in listing
            .get("zones")
            .and_then(Value::as_array)
            .ok_or("sample scope list unavailable")?
        {
            if zone.get("deleted").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let identity = zone.get("zoneID").ok_or("sample scope identity missing")?;
            let name = identity
                .get("zoneName")
                .and_then(Value::as_str)
                .ok_or("sample scope name missing")?;
            let category = if name == "PrimarySync" {
                "primary"
            } else if name.starts_with("SharedSync-") {
                "shared_photos"
            } else {
                continue;
            };
            if !scopes
                .iter()
                .any(|(prior, _, _): &(&str, String, Value)| *prior == category)
            {
                scopes.push((category, endpoint.clone(), identity.clone()));
            }
        }
    }
    if scopes.len() != 2 {
        return Err("both Photos scopes required for signature samples");
    }
    let mut total_transferred = 0;
    for (category, endpoint, identity) in scopes {
        let zone_name = identity
            .get("zoneName")
            .and_then(Value::as_str)
            .ok_or("sample zone missing")?
            .to_owned();
        let probe = CycleProbe {
            readonly: readonly.clone(),
            state: Arc::new(CycleProbeState::default()),
        };
        let list_type = "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted";
        let album = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::from([
                    ("remapEnums".to_owned(), Value::Bool(true)),
                    ("getCurrentSyncToken".to_owned(), Value::Bool(true)),
                ])),
                service_endpoint: endpoint.into(),
                name: "".into(),
                list_type: list_type.into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter: None,
                page_size: 3,
                zone_id: Arc::new(identity),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(probe.clone()),
        );
        let candidates = album
            .photos(Some(3))
            .await
            .map_err(|_error| "bounded rendition sample query failed")?;
        if candidates.len() > 3 {
            return Err("sample query exceeded asset bound");
        }
        let eligible = |asset: &crate::icloud::photos::PhotoAsset| {
            asset
                .versions()
                .iter()
                .try_fold(0_u64, |sum, (_, version)| sum.checked_add(version.size))
                .is_some_and(|size| size > 0 && size <= CYCLE_TRANSFER_CAP as u64)
        };
        let selected = candidates
            .into_iter()
            .filter(eligible)
            .max_by_key(|asset| {
                let types: std::collections::HashSet<_> = asset
                    .versions()
                    .iter()
                    .map(|(_, v)| v.asset_type.as_ref())
                    .collect();
                (types.len(), asset.versions().len())
            })
            .ok_or("no bounded downloadable rendition sample in scope")?;
        *probe
            .state
            .collection
            .lock()
            .map_err(|_error| "sample state unavailable")? = Some((list_type.to_owned(), None));
        let confirmed = album
            .confirm_catalog_asset(selected.asset_record_name())
            .await
            .map_err(|_error| "targeted current sample confirmation failed")?;
        if !eligible(&confirmed.asset) {
            return Err("confirmed sample exceeds advertised byte bound");
        }
        let isolated =
            tempfile::tempdir().map_err(|_error| "isolated sample directory unavailable")?;
        let directory = isolated.path().join("media");
        let state_path = isolated.path().join("state.sqlite");
        let budget = TransferBudget::new(directory.clone())?;
        for (resolution, live_resolution) in [
            (PhotoResolution::Original, AssetVersionSize::Original),
            (PhotoResolution::Medium, AssetVersionSize::Medium),
            (PhotoResolution::Thumb, AssetVersionSize::Thumb),
        ] {
            let db = Arc::new(
                SqliteStateDb::open_owned(&state_path, &owner)
                    .await
                    .map_err(|_error| "owned sample state unavailable")?,
            );
            let mut config = DownloadConfig::test_default();
            config.directory = directory.clone().into();
            config.folder_structure = "none".into();
            config.library = zone_name.clone().into();
            config.resolution = resolution;
            config.live_resolution = live_resolution;
            config.edited = true;
            config.alternative = true;
            config.concurrent_downloads = 1;
            config.retry.max_retries = 0;
            config.state_db = Some(db);
            let config = Arc::new(config);
            // This is deliberately a targeted rendition pipeline diagnostic,
            // not a normal baseline/checkpoint qualification.
            let result = super::pipeline::stream_and_download_from_stream(
                session.download_client(),
                futures_util::stream::iter(vec![Ok::<_, anyhow::Error>(confirmed.asset.clone())]),
                &config,
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                super::pipeline::StreamRuntime::new(None, None),
            )
            .await
            .map_err(|_error| "isolated rendition sample transfer failed")?;
            if !result.failed.is_empty()
                || result.state_write_failures != 0
                || result.enumeration_errors != 0
            {
                return Err("isolated rendition sample incomplete");
            }
        }
        let db = SqliteStateDb::open_owned(&state_path, &owner)
            .await
            .map_err(|_error| "sample reopen failed")?;
        report_v1_digest_evidence(&probe, &db, category).await?;
        if budget.used() == 0 {
            return Err("no rendition bytes sampled");
        }
        total_transferred += budget.used();
        if total_transferred > 2 * CYCLE_TRANSFER_CAP {
            return Err("combined rendition budget exceeded");
        }
        eprintln!(
            "scope={category} validation=direct_rendition_sample normal_cycle_proof=false transferred_bytes={} per_scope_cap={} total_cap={}",
            budget.used(),
            CYCLE_TRANSFER_CAP,
            2 * CYCLE_TRANSFER_CAP
        );
    }
    Ok(())
}
