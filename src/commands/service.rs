use std::path::Path;

use anyhow::Context;

use crate::auth;
use crate::config;
use crate::icloud;
use crate::password::SecretString;
use crate::retry;

/// Maximum number of re-authentication attempts before giving up.
pub(crate) const MAX_REAUTH_ATTEMPTS: u32 = 3;

/// iCloud web-client build identifiers sent with every CloudKit API request.
/// Apple embeds these in the JS bundle served by `icloud.com`. To find updated
/// values: open `icloud.com/photos` in a browser, inspect any CloudKit XHR, and
/// read `clientBuildNumber` / `clientMasteringNumber` from the query string.
const ICLOUD_CLIENT_BUILD_NUMBER: &str = "2522Project44";
const ICLOUD_CLIENT_MASTERING_NUMBER: &str = "2522B2";

/// Initialize the photos service with one 421 recovery attempt.
///
/// On 421 Misdirected Request, resets the HTTP/2 connection pool and retries
/// once. A second 421 surfaces `ICloudError::MisdirectedRequest` to the
/// caller; `sync_loop` routes both 421 and 401 through the same SRP re-auth
/// path (covering the case where stale session routing headers are pinning
/// the request to the wrong partition).
pub(crate) async fn init_photos_service(
    mut auth_result: auth::AuthResult,
    api_retry_config: retry::RetryConfig,
) -> anyhow::Result<(auth::SharedSession, icloud::photos::PhotosService)> {
    if auth_result.data.i_cdp_enabled {
        anyhow::bail!(
            "Advanced Data Protection (ADP) is enabled on this account.\n\n\
             ADP blocks the web API that kei uses to access photos.\n\
             To use kei, change both settings on your iPhone/iPad:\n  \
             1. Disable ADP: Settings > Apple ID > iCloud > Advanced Data Protection\n  \
             2. Enable web access: Settings > Apple ID > iCloud > Access iCloud Data on the Web"
        );
    }

    let ckdatabasews_url = auth_result
        .data
        .webservices
        .as_ref()
        .and_then(|ws| ws.ckdatabasews.as_ref())
        .map(|ep| ep.url.clone())
        .ok_or_else(|| anyhow::anyhow!("No ckdatabasews URL"))?;

    // Persist the active ckdatabasews URL so validate_session can detect
    // partition changes during watch-mode revalidation.
    auth_result
        .session
        .session_data
        .insert("ckdatabasews_url".to_owned(), ckdatabasews_url.clone());

    let client_id = auth_result
        .session
        .client_id()
        .unwrap_or_default()
        .to_owned();
    let dsid = auth_result
        .data
        .ds_info
        .as_ref()
        .and_then(|ds| ds.dsid.clone());
    let params = build_photos_params(&client_id, dsid.as_deref());

    let shared_session: auth::SharedSession =
        std::sync::Arc::new(tokio::sync::RwLock::new(auth_result.session));
    let session_box: Box<dyn icloud::photos::PhotosSession> = Box::new(shared_session.clone());

    tracing::debug!("Initializing photos service...");
    match icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        session_box,
        params.clone(),
        api_retry_config,
    )
    .await
    {
        Ok(service) => return Ok((shared_session, service)),
        Err(e) if !is_misdirected_request(&e) => return Err(e.into()),
        Err(_) => {}
    }

    // 421 Misdirected Request: Apple's CDN routed our HTTP/2 connection to
    // the wrong CloudKit partition. Per RFC 9110, the correct response is a
    // fresh connection — not re-auth. Try that once; if the second attempt
    // also 421s, surface `MisdirectedRequest` so `sync_loop` can invalidate
    // the cache and force SRP (where stale routing headers are the likely
    // cause).
    tracing::warn!(
        url = %ckdatabasews_url,
        "Service returned 421 Misdirected Request, retrying with fresh connection pool"
    );
    {
        let mut session = shared_session.write().await;
        session.reset_http_clients()?;
    }

    let session_box: Box<dyn icloud::photos::PhotosSession> = Box::new(shared_session.clone());
    let service = icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        session_box,
        params,
        api_retry_config,
    )
    .await?;
    Ok((shared_session, service))
}

/// Check if an iCloud error is a 421 Misdirected Request from the CloudKit service.
///
/// This happens when the HTTP/2 connection is routed to a CloudKit partition
/// server that cannot serve the user's data. Root cause may be stale
/// connection routing or stale session state; see `init_photos_service`.
fn is_misdirected_request(err: &icloud::error::ICloudError) -> bool {
    matches!(err, icloud::error::ICloudError::MisdirectedRequest)
}

/// Attempt to re-authenticate the session.
///
/// First validates the existing session; if invalid, performs full re-authentication.
/// If 2FA is required in headless mode, returns `AuthError::TwoFactorRequired`
/// so the caller can fire a notification and skip the current cycle.
///
/// # Lock strategy
///
/// A write lock is held across the `validate_session` call because validation
/// mutates the session (refreshes tokens). The lock is dropped before the
/// heavier `authenticate` call to avoid blocking download tasks. A 30-second
/// timeout guards against a hung validation request holding the lock
/// indefinitely.
pub(crate) async fn attempt_reauth<F>(
    shared_session: &auth::SharedSession,
    cookie_directory: &Path,
    username: &str,
    domain: &str,
    password_provider: &F,
) -> anyhow::Result<()>
where
    F: Fn() -> Option<SecretString>,
{
    let mut session = shared_session.write().await;

    // Try validation first — timeout prevents a hung HTTP request from
    // holding the write lock indefinitely and starving download tasks.
    let valid = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        auth::validate_session(&mut session, domain),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Session validation timed out after 30s"))??;
    if valid {
        tracing::debug!("Session still valid after re-validation");
        return Ok(());
    }

    tracing::info!("Session invalid, performing full re-authentication...");
    session.release_lock()?;
    drop(session);

    let new_auth = auth::authenticate(
        cookie_directory,
        username,
        password_provider,
        domain,
        None,
        None,
        None, // no code — interactive prompt or TwoFactorRequired
    )
    .await?;

    let mut session = shared_session.write().await;
    *session = new_auth.session;
    tracing::info!("Re-authentication successful");
    Ok(())
}

/// Interval between polls when waiting for a 2FA code submission.
const TWO_FA_POLL_SECS: u64 = 5;

/// Wait for `submit-code` to update the session file, with no network traffic.
///
/// Polls the session file's modification time every 5 seconds. When
/// `submit-code` trusts the session it writes updated cookies/session data,
/// changing the mtime and breaking the loop.
async fn wait_for_2fa_submit(cookie_dir: &Path, username: &str) {
    let session_path = auth::session_file_path(cookie_dir, username);
    let initial_mtime = tokio::fs::metadata(&session_path)
        .await
        .and_then(|m| m.modified())
        .ok();

    tracing::info!("Waiting for 2FA code submission...");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(TWO_FA_POLL_SECS)).await;

        let current_mtime = tokio::fs::metadata(&session_path)
            .await
            .and_then(|m| m.modified())
            .ok();
        if current_mtime != initial_mtime {
            tracing::debug!("Session file updated, retrying authentication");
            break;
        }
    }
}

/// Wait for a 2FA code submission, then retry authentication with back-off.
///
/// Polls `wait_for_2fa_submit` in a loop. After each mtime change, retries
/// the provided `auth_fn` up to 3 times with 5-second back-off to handle
/// lock contention (submit-code may still be running when mtime changes).
/// False wakeups from get-code's SRP writes (which change the mtime before
/// the session is trusted) are handled by looping back to wait.
pub(crate) async fn wait_and_retry_2fa<T, F, Fut>(
    cookie_dir: &Path,
    username: &str,
    auth_fn: F,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    loop {
        wait_for_2fa_submit(cookie_dir, username).await;

        // Invalidate the validation cache so authenticate() actually checks
        // with Apple instead of returning stale cached data from before 2FA.
        let sanitized: String = username
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let cache_path = cookie_dir.join(format!("{sanitized}.cache"));
        if cache_path.exists() {
            if let Err(e) = tokio::fs::remove_file(&cache_path).await {
                tracing::debug!(error = %e, "Could not remove validation cache");
            }
        }

        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(TWO_FA_POLL_SECS)).await;
            }
            match (auth_fn)().await {
                Ok(result) => return Ok(result),
                Err(e)
                    if e.downcast_ref::<auth::error::AuthError>()
                        .is_some_and(auth::error::AuthError::is_two_factor_required) =>
                {
                    tracing::debug!("Session not yet trusted, continuing to wait...");
                    break; // Back to outer loop (wait_for_2fa_submit)
                }
                Err(e)
                    if e.downcast_ref::<auth::error::AuthError>()
                        .is_some_and(auth::error::AuthError::is_lock_contention) =>
                {
                    tracing::debug!("Lock held by another process, retrying...");
                }
                Err(e) => return Err(e),
            }
        }
        tracing::debug!("Lock still held after retries, resuming wait...");
    }
}

/// Retry an auth operation on lock contention, with a brief wait.
///
/// Short-lived commands like `login get-code` and `login submit-code` may
/// collide with a `sync` process that is mid-auth (SRP takes a few seconds).
/// Instead of failing immediately, wait for the lock to be released.
pub(super) async fn retry_on_lock_contention<T, F, Fut>(auth_fn: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    const MAX_ATTEMPTS: u32 = 6;
    const DELAY_SECS: u64 = 3;

    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match (auth_fn)().await {
            Ok(result) => return Ok(result),
            Err(e)
                if e.downcast_ref::<auth::error::AuthError>()
                    .is_some_and(auth::error::AuthError::is_lock_contention) =>
            {
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = MAX_ATTEMPTS,
                    "Another kei process is holding the session lock, retrying..."
                );
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_secs(DELAY_SECS)).await;
            }
            Err(e) => return Err(e),
        }
    }
    #[allow(
        clippy::expect_used,
        reason = "loop runs MAX_ATTEMPTS >= 1 times; last_err set on every iteration"
    )]
    Err(last_err.expect("MAX_ATTEMPTS must be >= 1"))
}

/// Build the query parameters `HashMap` for the iCloud Photos `CloudKit` API.
pub(crate) fn build_photos_params(
    client_id: &str,
    dsid: Option<&str>,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::with_capacity(4);
    params.insert(
        "clientBuildNumber".into(),
        ICLOUD_CLIENT_BUILD_NUMBER.into(),
    );
    params.insert(
        "clientMasteringNumber".into(),
        ICLOUD_CLIENT_MASTERING_NUMBER.into(),
    );
    params.insert("clientId".into(), client_id.into());
    if let Some(dsid) = dsid {
        params.insert("dsid".into(), dsid.into());
    }
    params
}

/// Resolve a `LibrarySelection` into concrete `PhotoLibrary` instances.
pub(crate) async fn resolve_libraries(
    selection: &config::LibrarySelection,
    photos_service: &mut icloud::photos::PhotosService,
) -> anyhow::Result<Vec<icloud::photos::PhotoLibrary>> {
    match selection {
        config::LibrarySelection::All => {
            tracing::debug!("Using all available libraries");
            photos_service.all_libraries().await
        }
        config::LibrarySelection::Single(name) => {
            if name != "PrimarySync" {
                tracing::debug!(library = %name, "Using non-default library");
            }
            Ok(vec![photos_service.get_library(name).await?.clone()])
        }
    }
}

/// One pass through a specific album (or the library-wide pseudo-album).
///
/// `exclude_ids` is the per-pass set of asset IDs to filter out. Most passes
/// carry an empty set; the library-wide pass may pre-populate it with every
/// album member (for the `-a all` + `{album}` unfiled pass) or with excluded
/// albums' members (for `--exclude-album` without `--album`).
pub(crate) struct AlbumPass {
    pub album: icloud::photos::PhotoAlbum,
    pub exclude_ids: std::sync::Arc<rustc_hash::FxHashSet<String>>,
}

/// Ordered list of download passes for a single library.
#[derive(Debug)]
pub(crate) struct AlbumPlan {
    pub passes: Vec<AlbumPass>,
}

impl std::fmt::Debug for AlbumPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlbumPass")
            .field("album_name", &self.album.name)
            .field("exclude_ids_len", &self.exclude_ids.len())
            .finish()
    }
}

fn empty_exclude_ids() -> std::sync::Arc<rustc_hash::FxHashSet<String>> {
    std::sync::Arc::new(rustc_hash::FxHashSet::default())
}

/// Enumerate every asset ID in an album into `into`. Used both for the
/// legacy `--exclude-album` pre-fetch and for computing the "already in
/// some album" set that gates the `-a all` + `{album}` unfiled pass.
async fn collect_album_asset_ids(
    album: &icloud::photos::PhotoAlbum,
    into: &mut rustc_hash::FxHashSet<String>,
) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    // Propagate `len()` failures — the count is load-bearing for the
    // `-a all` + `{album}` unfiled pass: a silent 0 here leaves the
    // exclusion set incomplete and the unfiled pass re-downloads assets
    // that are already in some user album.
    let count = album
        .len()
        .await
        .with_context(|| format!("failed to get asset count for album '{}'", album.name))?;
    let (stream, _token_rx) = album.photo_stream_with_token(None, Some(count), 1);
    tokio::pin!(stream);
    while let Some(result) = stream.next().await {
        let asset = result?;
        into.insert(asset.id().to_string());
    }
    Ok(())
}

/// Resolve the full download plan for a library.
///
/// The returned [`AlbumPlan`] contains one or more passes, each paired with
/// its own exclude-asset-ids set:
///
/// - [`config::AlbumSelection::LibraryOnly`] returns a single library-wide pass.
///   `--exclude-album X` without `--album` populates the pass's exclude set
///   with X's members so they don't leak through the library-wide stream.
/// - [`config::AlbumSelection::Named`] returns one pass per matched album, minus
///   anything in `exclude_albums`. Missing names are a hard error.
/// - [`config::AlbumSelection::All`] returns one pass per discovered album (minus
///   `exclude_albums`). When `{album}` is in `folder_structure`, an extra
///   library-wide "unfiled" pass is appended; its exclude set is the union
///   of every discovered album's members (including excluded ones — users
///   explicitly asked to skip those, so they must not fall through to the
///   unfiled pass either).
///
/// `folder_structure` is consulted only for `config::AlbumSelection::All`, to decide
/// whether to add the unfiled pass.
pub(crate) async fn resolve_albums(
    library: &icloud::photos::PhotoLibrary,
    selection: &config::AlbumSelection,
    exclude_albums: &[String],
    folder_structure: &str,
) -> anyhow::Result<AlbumPlan> {
    let empty = empty_exclude_ids();

    match selection {
        config::AlbumSelection::LibraryOnly => {
            if exclude_albums.is_empty() {
                return Ok(AlbumPlan {
                    passes: vec![AlbumPass {
                        album: library.all(),
                        exclude_ids: empty,
                    }],
                });
            }
            // Legacy: --exclude-album without --album. Pre-collect IDs so
            // they're filtered from the library-wide stream.
            let album_map = library.albums().await?;
            let mut exclude_ids = rustc_hash::FxHashSet::default();
            for name in exclude_albums {
                if let Some(album) = album_map.get(name.as_str()) {
                    tracing::debug!(album = name, "Pre-fetching excluded album asset IDs");
                    collect_album_asset_ids(album, &mut exclude_ids).await?;
                } else {
                    tracing::warn!(album = name, "Excluded album not found, ignoring");
                }
            }
            tracing::debug!(count = exclude_ids.len(), "Collected excluded asset IDs");
            Ok(AlbumPlan {
                passes: vec![AlbumPass {
                    album: library.all(),
                    exclude_ids: std::sync::Arc::new(exclude_ids),
                }],
            })
        }
        config::AlbumSelection::Named(names) => {
            // Dedup names: passing the same album twice should collapse to
            // a single pass, not error out after the first remove()
            // drained the map.
            let mut album_map = library.albums().await?;
            let mut passes = Vec::new();
            let mut seen = rustc_hash::FxHashSet::default();
            for name in names {
                if !seen.insert(name.as_str()) {
                    continue;
                }
                if exclude_albums.iter().any(|e| e == name) {
                    tracing::debug!(album = name, "Album excluded by --exclude-album");
                    continue;
                }
                if let Some(album) = album_map.remove(name.as_str()) {
                    passes.push(AlbumPass {
                        album,
                        exclude_ids: std::sync::Arc::clone(&empty),
                    });
                } else {
                    let available: Vec<&String> = album_map.keys().collect();
                    anyhow::bail!("Album '{name}' not found. Available albums: {available:?}");
                }
            }
            Ok(AlbumPlan { passes })
        }
        config::AlbumSelection::All => {
            // Smart folders (Favorites, Recently Deleted, Hidden, etc.) are
            // skipped: the feature request asked for user-created albums,
            // and surfacing Apple's system folders as download targets
            // creates confusing trees. Users who want one can still name
            // it explicitly via `-a Favorites`.
            let smart_folder_names: rustc_hash::FxHashSet<&'static str> =
                icloud::photos::smart_folders::smart_folders()
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect();
            let album_map: std::collections::HashMap<String, icloud::photos::PhotoAlbum> = library
                .albums()
                .await?
                .into_iter()
                .filter(|(name, _)| !smart_folder_names.contains(name.as_str()))
                .collect();
            let has_album_token = folder_structure.contains("{album}");

            // Collect every user album's members (including --exclude-album
            // ones) into the unfiled exclusion set before consuming the
            // map. Excluded albums must contribute too, otherwise their
            // photos would leak out through the library-wide unfiled pass.
            // Fetched in parallel because this runs before the first byte
            // is downloaded; for libraries with many albums the serial
            // version added minutes of startup latency.
            let mut in_any_album: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
            if has_album_token {
                use futures_util::{StreamExt, TryStreamExt};
                const EXCLUSION_FETCH_CONCURRENCY: usize = 8;
                let per_album: Vec<rustc_hash::FxHashSet<String>> =
                    futures_util::stream::iter(album_map.iter())
                        .map(|(name, album)| async move {
                            tracing::debug!(
                                album = %name,
                                "Pre-fetching IDs for unfiled exclusion set"
                            );
                            let mut set = rustc_hash::FxHashSet::default();
                            collect_album_asset_ids(album, &mut set).await?;
                            anyhow::Ok(set)
                        })
                        .buffer_unordered(EXCLUSION_FETCH_CONCURRENCY)
                        .try_collect()
                        .await?;
                for set in per_album {
                    in_any_album.extend(set);
                }
            }

            // Sort by name — HashMap iteration is non-deterministic and
            // logs/dry-run output should be stable across runs.
            let mut named_albums: Vec<(String, icloud::photos::PhotoAlbum)> =
                album_map.into_iter().collect();
            named_albums.sort_by(|a, b| a.0.cmp(&b.0));

            let mut passes: Vec<AlbumPass> = Vec::new();
            let mut excluded_count = 0usize;
            for (name, album) in named_albums {
                if exclude_albums.iter().any(|e| e == &name) {
                    excluded_count += 1;
                    tracing::debug!(album = %name, "Album excluded by --exclude-album");
                    continue;
                }
                passes.push(AlbumPass {
                    album,
                    exclude_ids: std::sync::Arc::clone(&empty),
                });
            }
            tracing::info!(
                count = passes.len(),
                excluded = excluded_count,
                unfiled_pass = has_album_token,
                "Expanded '-a all' to every user-created album (smart folders skipped)"
            );
            if has_album_token {
                passes.push(AlbumPass {
                    album: library.all(),
                    exclude_ids: std::sync::Arc::new(in_any_album),
                });
            }
            Ok(AlbumPlan { passes })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_photos_params tests ───────────────────────────────────────

    #[test]
    fn build_photos_params_includes_client_id_and_dsid() {
        let params = build_photos_params("test-client-id-123", Some("99999"));

        assert_eq!(
            params.get("clientBuildNumber"),
            Some(&serde_json::Value::String(
                ICLOUD_CLIENT_BUILD_NUMBER.to_string()
            ))
        );
        assert_eq!(
            params.get("clientMasteringNumber"),
            Some(&serde_json::Value::String(
                ICLOUD_CLIENT_MASTERING_NUMBER.to_string()
            ))
        );
        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String("test-client-id-123".to_string()))
        );
        assert_eq!(
            params.get("dsid"),
            Some(&serde_json::Value::String("99999".to_string()))
        );
    }

    #[test]
    fn build_photos_params_no_dsid() {
        let params = build_photos_params("client-abc", None);

        assert!(!params.contains_key("dsid"));
        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String("client-abc".to_string()))
        );
    }

    #[test]
    fn build_photos_params_empty_client_id() {
        let params = build_photos_params("", Some("12345"));

        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String(String::new()))
        );
        assert_eq!(
            params.get("dsid"),
            Some(&serde_json::Value::String("12345".to_string()))
        );
    }

    // ── resolve_albums tests ──────────────────────────────────────────

    use crate::icloud::photos::PhotoLibrary;
    use crate::test_helpers::MockPhotosSession;

    /// Build a `PhotoLibrary` stub with a preconfigured mock session.
    fn stub_library(mock: MockPhotosSession) -> PhotoLibrary {
        PhotoLibrary::new_stub(Box::new(mock))
    }

    /// CloudKit folder record for a user album. The albumNameEnc field is
    /// base64-encoded.
    fn folder_record(record_name: &str, album_name: &str) -> serde_json::Value {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(album_name);
        serde_json::json!({
            "recordName": record_name,
            "recordType": "CPLAlbumByPositionLive",
            "fields": {
                "albumNameEnc": {"value": encoded},
                "isDeleted": {"value": false}
            }
        })
    }

    /// A single paired CPLMaster+CPLAsset page for photo streaming.
    fn asset_page(record_name: &str) -> serde_json::Value {
        serde_json::json!({
            "records": [
                {
                    "recordName": record_name,
                    "recordType": "CPLMaster",
                    "fields": {
                        "filenameEnc": {"value": "dGVzdC5qcGc=", "type": "STRING"},
                        "resOriginalRes": {"value": {
                            "downloadURL": "https://example.com/photo.jpg",
                            "size": 1024,
                            "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                        }},
                        "resOriginalFileType": {"value": "public.jpeg"},
                        "itemType": {"value": "public.jpeg"},
                        "adjustmentRenderType": {"value": 0, "type": "INT64"}
                    }
                },
                {
                    "recordName": format!("asset-{record_name}"),
                    "recordType": "CPLAsset",
                    "fields": {
                        "masterRef": {
                            "value": {"recordName": record_name, "zoneID": {"zoneName": "PrimarySync"}},
                            "type": "REFERENCE"
                        },
                        "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                        "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                    }
                }
            ]
        })
    }

    /// Batch album count response.
    fn album_count_response(count: u64) -> serde_json::Value {
        serde_json::json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
        })
    }

    // Shortcut for building an AlbumSelection::Named from string literals.
    fn named(names: &[&str]) -> config::AlbumSelection {
        config::AlbumSelection::Named(names.iter().map(|s| (*s).to_string()).collect())
    }

    #[tokio::test]
    async fn resolve_albums_no_album_no_exclude() {
        let mock = MockPhotosSession::new();
        let library = stub_library(mock);
        let plan = resolve_albums(
            &library,
            &config::AlbumSelection::LibraryOnly,
            &[],
            "%Y/%m/%d",
        )
        .await
        .unwrap();
        assert_eq!(plan.passes.len(), 1, "should return library.all()");
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_albums_exclude_not_found_warns() {
        // fetch_folders returns no albums, but we exclude "Nonexistent"
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);

        let plan = resolve_albums(
            &library,
            &config::AlbumSelection::LibraryOnly,
            &["Nonexistent".to_string()],
            "%Y/%m/%d",
        )
        .await
        .unwrap();
        assert_eq!(plan.passes.len(), 1, "should return library.all()");
        assert!(
            plan.passes[0].exclude_ids.is_empty(),
            "non-existent album produces no IDs"
        );
    }

    #[tokio::test]
    async fn resolve_albums_explicit_album_found() {
        // fetch_folders returns "Vacation" album
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);

        let plan = resolve_albums(&library, &named(&["Vacation"]), &[], "%Y/%m/%d")
            .await
            .unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_albums_explicit_album_not_found_errors() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);

        let result = resolve_albums(&library, &named(&["DoesNotExist"]), &[], "%Y/%m/%d").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn resolve_albums_dedups_duplicate_names() {
        // `--album Vacation --album Vacation` should resolve to a single album,
        // not error after the first instance drains the map.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);

        let plan = resolve_albums(&library, &named(&["Vacation", "Vacation"]), &[], "%Y/%m/%d")
            .await
            .unwrap();
        assert_eq!(plan.passes.len(), 1, "duplicate names dedup to 1");
    }

    #[tokio::test]
    async fn resolve_albums_explicit_album_with_exclusion() {
        // Two albums: Vacation and Hidden. Exclude Hidden.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Hidden")
        ]}));
        let library = stub_library(mock);

        let plan = resolve_albums(
            &library,
            &named(&["Vacation", "Hidden"]),
            &["Hidden".to_string()],
            "%Y/%m/%d",
        )
        .await
        .unwrap();
        assert_eq!(
            plan.passes.len(),
            1,
            "Hidden should be excluded from matched albums"
        );
        assert!(
            plan.passes[0].exclude_ids.is_empty(),
            "explicit album path doesn't populate exclude IDs"
        );
    }

    #[tokio::test]
    async fn resolve_albums_exclude_without_album_collects_ids() {
        // The mock session needs to handle:
        // 1. fetch_folders → returns album "Hidden"
        // 2. album.len() → returns count
        // 3. photo_stream fetcher → returns one asset page
        // 4. photo_stream fetcher 2nd call → returns empty (end of stream)
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [
                folder_record("FOLDER_1", "Hidden")
            ]}))
            .ok(album_count_response(1))
            .ok(asset_page("MASTER_1"))
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);

        let plan = resolve_albums(
            &library,
            &config::AlbumSelection::LibraryOnly,
            &["Hidden".to_string()],
            "%Y/%m/%d",
        )
        .await
        .unwrap();
        assert_eq!(plan.passes.len(), 1, "should return library.all()");
        assert!(
            plan.passes[0].exclude_ids.contains("MASTER_1"),
            "should contain the excluded asset ID"
        );
    }

    #[tokio::test]
    async fn resolve_albums_all_expands_to_every_album() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Summer Trip")
        ]}));
        let library = stub_library(mock);

        let plan = resolve_albums(&library, &config::AlbumSelection::All, &[], "%Y/%m/%d")
            .await
            .unwrap();
        assert_eq!(
            plan.passes.len(),
            2,
            "every user-created album becomes a pass, no unfiled pass without {{album}}"
        );
        for pass in &plan.passes {
            assert!(
                pass.exclude_ids.is_empty(),
                "concrete album passes carry no exclusion"
            );
        }
    }

    #[tokio::test]
    async fn resolve_albums_all_with_album_token_adds_unfiled_pass() {
        // fetch_folders returns one album; then len + stream mocks so the
        // unfiled exclusion set is populated from that album's member.
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [folder_record("FOLDER_1", "Vacation")]}))
            .ok(album_count_response(1))
            .ok(asset_page("MASTER_1"))
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);

        let plan = resolve_albums(
            &library,
            &config::AlbumSelection::All,
            &[],
            "{album}/%Y/%m/%d",
        )
        .await
        .unwrap();
        assert_eq!(
            plan.passes.len(),
            2,
            "concrete album + unfiled pass when {{album}} is in template"
        );
        assert!(
            plan.passes[0].exclude_ids.is_empty(),
            "concrete pass carries no exclusion"
        );
        assert!(
            plan.passes[1].exclude_ids.contains("MASTER_1"),
            "unfiled pass excludes assets already in some album"
        );
    }

    #[tokio::test]
    async fn resolve_albums_all_respects_exclude_albums() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Hidden Trip")
        ]}));
        let library = stub_library(mock);

        let plan = resolve_albums(
            &library,
            &config::AlbumSelection::All,
            &["Hidden Trip".to_string()],
            "%Y/%m/%d",
        )
        .await
        .unwrap();
        assert_eq!(
            plan.passes.len(),
            1,
            "Hidden Trip is filtered out of the concrete passes"
        );
    }

    // ── is_misdirected_request tests ──────────────────────────────────

    #[test]
    fn misdirected_request_variant_detected() {
        let err = icloud::error::ICloudError::MisdirectedRequest;
        assert!(is_misdirected_request(&err));
    }

    #[test]
    fn non_421_connection_error_not_misdirected() {
        let err = icloud::error::ICloudError::Connection("HTTP 500 ...".to_string());
        assert!(!is_misdirected_request(&err));
    }

    #[test]
    fn session_expired_not_misdirected() {
        let err = icloud::error::ICloudError::SessionExpired { status: 401 };
        assert!(!is_misdirected_request(&err));
    }

    #[test]
    fn service_not_activated_not_misdirected() {
        let err = icloud::error::ICloudError::ServiceNotActivated {
            code: "ZONE_NOT_FOUND".to_string(),
            reason: "zone not found".to_string(),
        };
        assert!(!is_misdirected_request(&err));
    }

    #[tokio::test]
    async fn resolve_albums_same_album_in_both_yields_empty() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);

        let plan = resolve_albums(
            &library,
            &named(&["Vacation"]),
            &["Vacation".to_string()],
            "%Y/%m/%d",
        )
        .await
        .unwrap();
        assert!(
            plan.passes.is_empty(),
            "album present in both --album and --exclude-album should yield zero albums"
        );
    }
}
