use std::path::Path;

use anyhow::Context;

use crate::auth;
use crate::icloud;
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
    let service = match icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        session_box,
        params,
        api_retry_config,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            // The pool-reset retry also failed. Surface it before bubbling
            // so watch-mode operators can correlate reauth cycles with
            // sustained CloudKit partition issues.
            tracing::warn!(
                url = %ckdatabasews_url,
                attempt = 2,
                error = %e,
                "init_photos_service retry after pool reset failed; \
                 sync_loop will fall through to full SRP re-auth"
            );
            return Err(e.into());
        }
    };
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
pub(crate) async fn attempt_reauth(
    shared_session: &auth::SharedSession,
    cookie_directory: &Path,
    username: &str,
    domain: &str,
    password_provider: &crate::password::PasswordProvider,
) -> anyhow::Result<()> {
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

/// Resolve a [`crate::selection::LibrarySelector`] into the concrete set of
/// `PhotoLibrary` instances the sync loop iterates over. Walks every zone the
/// account exposes (primary + private + shared), keeps the ones the
/// `primary` / `shared_all` flags or the `named` set include, drops any that
/// match `excluded`, and bails when a positive `named` entry resolves to
/// nothing.
///
/// `named` and `excluded` entries match a zone by exact zone name (case
/// insensitive) or by the truncated 8-char form
/// (`paths::truncate_library_zone`) so the on-disk `{library}` segment can
/// be copied straight back into `--library`.
pub(crate) async fn resolve_libraries(
    selector: &crate::selection::LibrarySelector,
    photos_service: &mut icloud::photos::PhotosService,
) -> anyhow::Result<Vec<icloud::photos::PhotoLibrary>> {
    use crate::download::paths::truncate_library_zone;

    // Fast path for the default `--library primary`: skip the private +
    // shared library HTTP listings. Saves two requests per sync for the
    // common single-library case.
    if selector == &crate::selection::LibrarySelector::default() {
        let lib = photos_service
            .get_library(crate::icloud::photos::PRIMARY_ZONE_NAME)
            .await?;
        return Ok(vec![lib.clone()]);
    }

    let all = photos_service.all_libraries().await?;

    // Track which selector entries actually matched so we can bail on a
    // positive miss (the spec's "Album 'Vacatiom' not found" rule, applied to
    // libraries). `named` only; `excluded` misses just warn.
    let mut named_hits: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(selector.named.len());
    let mut chosen: Vec<icloud::photos::PhotoLibrary> = Vec::new();

    for lib in &all {
        let zone = lib.zone_name();
        let truncated = truncate_library_zone(zone);
        let is_primary = zone == crate::icloud::photos::PRIMARY_ZONE_NAME;
        let is_shared = crate::icloud::photos::is_shared_zone(zone);

        let included = (selector.primary && is_primary)
            || (selector.shared_all && is_shared)
            || selector.named.iter().any(|entry| {
                let hit = library_entry_matches_zone(entry, zone, truncated);
                if hit {
                    named_hits.insert(entry.as_str());
                }
                hit
            });
        if !included {
            continue;
        }

        let excluded = selector.excluded.iter().any(|entry| {
            (entry.eq_ignore_ascii_case("primary") && is_primary)
                || (entry.eq_ignore_ascii_case("shared") && is_shared)
                || library_entry_matches_zone(entry, zone, truncated)
        });
        if excluded {
            continue;
        }

        // `all_libraries()` returns disjoint sets (primary, private minus
        // PrimarySync, shared) so no cross-source dedup is needed here.
        chosen.push(lib.clone());
    }

    if let Some(missed) = selector
        .named
        .iter()
        .find(|n| !named_hits.contains(n.as_str()))
    {
        let known: Vec<&str> = all
            .iter()
            .map(icloud::photos::PhotoLibrary::zone_name)
            .collect();
        anyhow::bail!(
            "--library '{missed}' not found. Available zones: {}. Run `kei list libraries` to see every zone with its truncated form.",
            known.join(", ")
        );
    }

    if chosen.is_empty() {
        anyhow::bail!(
            "--library resolved to zero libraries against this account; \
             pass at least one of primary / shared / a zone name without \
             excluding everything"
        );
    }

    // Path collision guard. `truncate_library_zone` keeps the first 8 hex
    // chars of `SharedSync-<UUID>`; CloudKit UUIDs are not guaranteed
    // unique on that prefix, so two distinct shared zones can render to
    // the same `{library}` path segment. If both land in the chosen set,
    // multi-library sync silently overwrites one zone's bytes with the
    // other's (state DB stays per-zone distinct under the v8 PK so the
    // corruption is invisible). Bail with both full UUIDs and the
    // truncated form so the user can pin a longer prefix via `--library
    // SharedSync-<longer>`.
    let mut by_truncated: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::with_capacity(chosen.len());
    for lib in &chosen {
        let zone = lib.zone_name();
        by_truncated
            .entry(truncate_library_zone(zone))
            .or_default()
            .push(zone);
    }
    if let Some((truncated, zones)) = by_truncated.iter().find(|(_, zones)| zones.len() > 1) {
        let mut sorted = zones.clone();
        sorted.sort_unstable();
        anyhow::bail!(
            "Multiple selected libraries collapse to the same `{{library}}` path \
             segment `{truncated}`: [{full}]. The 8-char shared-library prefix \
             collides; pin one of these zones via `--library <longer-prefix>` \
             (use `kei list libraries` to see every zone with its truncated form).",
            full = sorted.join(", "),
        );
    }

    match chosen.as_slice() {
        [only] if only.zone_name() != crate::icloud::photos::PRIMARY_ZONE_NAME => {
            tracing::debug!(library = %only.zone_name(), "Using non-default library");
        }
        [_] => {} // primary-only logged in the fast path above
        many => tracing::debug!(count = many.len(), "Using multiple libraries"),
    }

    Ok(chosen)
}

/// Match a `--library` entry (full zone name or truncated 8-char form)
/// against a live zone, case-insensitive. The truncated form is what
/// `{library}` renders into paths, so users can copy a path segment and
/// paste it back into `--library`.
fn library_entry_matches_zone(entry: &str, zone: &str, truncated: &str) -> bool {
    entry.eq_ignore_ascii_case(zone) || entry.eq_ignore_ascii_case(truncated)
}

/// Category of a download pass: a named user album, an Apple-defined smart
/// folder, or the library-wide unfiled pseudo-pass. Drives template/token
/// selection in the path renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassKind {
    Album,
    SmartFolder,
    Unfiled,
}

impl PassKind {
    /// Folder-structure token expanded for this pass kind. The unfiled pass
    /// reuses `{album}` so existing configs with `--folder-structure
    /// "{album}/..."` still produce the same on-disk tree (the token
    /// collapses to an empty segment when the unfiled pass runs with the
    /// library-wide pseudo-album's empty name).
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Album | Self::Unfiled => "{album}",
            Self::SmartFolder => "{smart-folder}",
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
    pub kind: PassKind,
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
            .field("kind", &self.kind)
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

/// Static set of Apple-defined smart-folder names. Looked up by name on
/// the same map `PhotoLibrary::albums` returns; used to split user albums
/// vs smart folders for the v0.13 selection model.
fn smart_folder_name_set() -> rustc_hash::FxHashSet<&'static str> {
    icloud::photos::smart_folders::smart_folders()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Resolve the v0.13 [`crate::selection::Selection`] into concrete download
/// passes for one library.
///
/// Per-category behaviour, mirroring the spec:
///
/// - `albums: None` → no album passes.
/// - `albums: All { excluded }` → one pass per user album in the library
///   except those listed in `excluded`. Missing exclusion names log a
///   warning and are skipped; smart folders are filtered out (use
///   `--smart-folder all` to opt in).
/// - `albums: Named { included, excluded }` → one pass per name in
///   `included`. Missing names bail at startup with the available album
///   list. Names that are smart folders bail with a hint to use
///   `--smart-folder` instead.
/// - `smart_folders: None` → no smart-folder passes.
/// - `smart_folders: All { include_sensitive, excluded }` → one pass per
///   smart folder; Hidden / Recently Deleted are skipped unless
///   `include_sensitive` is true. Missing exclusion names log a warning.
/// - `smart_folders: Named { included, excluded }` → one pass per smart
///   folder name; non-smart-folder names bail.
/// - `unfiled: true` → an extra library-wide pass with `exclude_ids`
///   covering every member of every selected album (the spec dedup
///   invariant: an asset in a selected album must not also land at the
///   unfiled path). Selected smart folders do not contribute to the
///   exclusion set — smart-folder membership is orthogonal to album
///   membership.
///
/// Album member IDs are fetched in parallel before the album map is
/// consumed into passes; each `PhotoAlbum` is moved into exactly one pass.
pub(crate) async fn resolve_passes(
    library: &icloud::photos::PhotoLibrary,
    selection: &crate::selection::Selection,
) -> anyhow::Result<AlbumPlan> {
    use crate::selection::{AlbumSelector, SmartFolderSelector};

    let album_active = !matches!(selection.albums, AlbumSelector::None);
    let smart_active = !matches!(selection.smart_folders, SmartFolderSelector::None);

    if !album_active && !smart_active && !selection.unfiled {
        return Ok(AlbumPlan { passes: Vec::new() });
    }

    let mut album_map = library.albums().await?;
    let smart_names = smart_folder_name_set();

    let selected_album_names = pick_album_names(&selection.albums, &album_map, &smart_names)?;
    let selected_smart_names =
        pick_smart_folder_names(&selection.smart_folders, &album_map, &smart_names)?;

    let unfiled_exclude_ids = if selection.unfiled {
        compute_unfiled_exclude_ids(&album_map, &selected_album_names).await?
    } else {
        rustc_hash::FxHashSet::default()
    };

    let empty = empty_exclude_ids();
    let mut passes: Vec<AlbumPass> = Vec::new();

    // Stable, alphabetised pass order so logs and dry-run output don't
    // jitter with HashMap iteration order.
    drain_named_into_passes(
        &mut album_map,
        selected_album_names,
        PassKind::Album,
        &empty,
        &mut passes,
    );
    drain_named_into_passes(
        &mut album_map,
        selected_smart_names,
        PassKind::SmartFolder,
        &empty,
        &mut passes,
    );

    if selection.unfiled {
        passes.push(AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: std::sync::Arc::new(unfiled_exclude_ids),
        });
    }

    Ok(AlbumPlan { passes })
}

/// Pick the user-album names selected by `albums`. Bails on missing
/// `Named` entries and on missing `excluded` entries (typo-only excludes
/// would otherwise let the unintended album through silently).
fn pick_album_names(
    selector: &crate::selection::AlbumSelector,
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<Vec<String>> {
    use crate::selection::AlbumSelector;
    match selector {
        AlbumSelector::None => Ok(Vec::new()),
        AlbumSelector::All { excluded } => {
            bail_unknown_excluded_albums(excluded, album_map, smart_names)?;
            Ok(album_map
                .keys()
                .filter(|name| {
                    !smart_names.contains(name.as_str()) && !excluded.contains(name.as_str())
                })
                .cloned()
                .collect())
        }
        AlbumSelector::Named { included, excluded } => {
            let mut chosen = Vec::with_capacity(included.len());
            for name in included {
                if smart_names.contains(name.as_str()) {
                    anyhow::bail!(
                        "'{name}' is a smart folder; pass `--smart-folder {name}` instead of `--album`"
                    );
                }
                if excluded.contains(name) {
                    continue;
                }
                if album_map.contains_key(name.as_str()) {
                    chosen.push(name.clone());
                } else {
                    let mut available: Vec<&String> = album_map
                        .keys()
                        .filter(|k| !smart_names.contains(k.as_str()))
                        .collect();
                    available.sort();
                    anyhow::bail!("Album '{name}' not found. Available albums: {available:?}");
                }
            }
            bail_unknown_excluded_albums(excluded, album_map, smart_names)?;
            Ok(chosen)
        }
    }
}

fn bail_unknown_excluded_albums(
    excluded: &std::collections::BTreeSet<String>,
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<()> {
    for name in excluded {
        if !album_map.contains_key(name.as_str()) {
            let mut available: Vec<&String> = album_map
                .keys()
                .filter(|k| !smart_names.contains(k.as_str()))
                .collect();
            available.sort();
            anyhow::bail!("Excluded album '{name}' not found. Available albums: {available:?}");
        }
    }
    Ok(())
}

/// Pick the smart-folder names selected by `smart_folders`. Bails on
/// `Named` entries that aren't smart folders; warns on missing
/// `excluded` entries.
fn pick_smart_folder_names(
    selector: &crate::selection::SmartFolderSelector,
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<Vec<String>> {
    use crate::selection::SmartFolderSelector;
    let sensitive: rustc_hash::FxHashSet<&'static str> =
        icloud::photos::smart_folders::sensitive_smart_folder_names().collect();

    match selector {
        SmartFolderSelector::None => Ok(Vec::new()),
        SmartFolderSelector::All {
            include_sensitive,
            excluded,
        } => {
            for name in excluded {
                warn_excluded_not_a_smart_folder(name, smart_names);
            }
            Ok(album_map
                .keys()
                .filter(|name| smart_names.contains(name.as_str()))
                .filter(|name| *include_sensitive || !sensitive.contains(name.as_str()))
                .filter(|name| !excluded.contains(name.as_str()))
                .cloned()
                .collect())
        }
        SmartFolderSelector::Named { included, excluded } => {
            let mut chosen = Vec::with_capacity(included.len());
            for name in included {
                if !smart_names.contains(name.as_str()) {
                    let mut available: Vec<&str> = smart_names.iter().copied().collect();
                    available.sort();
                    anyhow::bail!(
                        "'{name}' is not an Apple smart folder. Available: {available:?}"
                    );
                }
                if excluded.contains(name) {
                    continue;
                }
                if album_map.contains_key(name.as_str()) {
                    chosen.push(name.clone());
                } else {
                    tracing::warn!(
                        smart_folder = %name,
                        "Smart folder not present in this library, skipping"
                    );
                }
            }
            for name in excluded {
                warn_excluded_not_a_smart_folder(name, smart_names);
            }
            Ok(chosen)
        }
    }
}

fn warn_excluded_not_a_smart_folder(name: &str, smart_names: &rustc_hash::FxHashSet<&'static str>) {
    if !smart_names.contains(name) {
        tracing::warn!(
            smart_folder = %name,
            "Excluded smart folder is not an Apple smart folder, ignoring"
        );
    }
}

/// Fetch every selected album's member IDs in parallel. The PhotoAlbums
/// stay borrowed in the map; callers move them into passes after this
/// returns. Empty input → empty set, no fetches.
async fn compute_unfiled_exclude_ids(
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    selected_album_names: &[String],
) -> anyhow::Result<rustc_hash::FxHashSet<String>> {
    use futures_util::{StreamExt, TryStreamExt};
    const EXCLUSION_FETCH_CONCURRENCY: usize = 8;

    if selected_album_names.is_empty() {
        return Ok(rustc_hash::FxHashSet::default());
    }

    // Defensive: pick_album_names validated every name against album_map,
    // so a miss here means the map mutated between checks. Bail loudly
    // rather than silently shrinking the exclusion set, which would let
    // already-filed assets re-download under --unfiled.
    let mut tuples: Vec<(&String, &icloud::photos::PhotoAlbum)> =
        Vec::with_capacity(selected_album_names.len());
    for name in selected_album_names {
        match album_map.get(name.as_str()) {
            Some(album) => tuples.push((name, album)),
            None => anyhow::bail!(
                "Selected album '{name}' missing from library album map at \
                 unfiled-exclusion time"
            ),
        }
    }

    let per_album: Vec<rustc_hash::FxHashSet<String>> = futures_util::stream::iter(tuples)
        .map(|(name, album)| async move {
            tracing::debug!(album = %name, "Pre-fetching IDs for unfiled exclusion set");
            let mut set = rustc_hash::FxHashSet::default();
            collect_album_asset_ids(album, &mut set).await?;
            anyhow::Ok(set)
        })
        .buffer_unordered(EXCLUSION_FETCH_CONCURRENCY)
        .try_collect()
        .await?;

    let mut union = rustc_hash::FxHashSet::default();
    for set in per_album {
        union.extend(set);
    }
    Ok(union)
}

/// Move named entries out of `album_map` into `passes` in alphabetical
/// order, sharing `empty_excludes` as the per-pass exclude set. A `None`
/// from `remove` should be impossible (`pick_*` validates membership) but
/// is logged + skipped to keep the resolver out of unwrap territory.
fn drain_named_into_passes(
    album_map: &mut std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    mut names: Vec<String>,
    kind: PassKind,
    empty_excludes: &std::sync::Arc<rustc_hash::FxHashSet<String>>,
    passes: &mut Vec<AlbumPass>,
) {
    names.sort();
    for name in names {
        let Some(album) = album_map.remove(name.as_str()) else {
            tracing::warn!(category = ?kind, name = %name, "Selected entry disappeared from map, skipping");
            continue;
        };
        passes.push(AlbumPass {
            kind,
            album,
            exclude_ids: std::sync::Arc::clone(empty_excludes),
        });
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

    // ── resolve_passes tests ─────────────────────────────────────────

    use crate::icloud::photos::PhotoLibrary;
    use crate::selection::{AlbumSelector, Selection, SmartFolderSelector};
    use crate::test_helpers::MockPhotosSession;
    use std::collections::BTreeSet;

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

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn selection_with_albums(albums: AlbumSelector, unfiled: bool) -> Selection {
        Selection {
            albums,
            smart_folders: SmartFolderSelector::None,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled,
        }
    }

    #[tokio::test]
    async fn resolve_passes_unfiled_only_returns_library_wide_pass() {
        // No albums or smart folders, but unfiled = true: a single library
        // pass with no exclusion (today's `LibraryOnly` default).
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(AlbumSelector::None, true);

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_effective_empty_returns_no_passes() {
        // None + None + unfiled=false → no work; sync_loop exits cleanly.
        let mock = MockPhotosSession::new();
        let library = stub_library(mock);
        let sel = selection_with_albums(AlbumSelector::None, false);

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert!(plan.passes.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_named_album_found() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: BTreeSet::new(),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_named_album_missing_bails() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["DoesNotExist"]),
                excluded: BTreeSet::new(),
            },
            false,
        );

        let err = resolve_passes(&library, &sel).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "msg: {err}");
    }

    #[tokio::test]
    async fn resolve_passes_named_smart_folder_in_album_position_bails() {
        // `--album Favorites` should redirect users to `--smart-folder`.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            false,
        );

        let err = resolve_passes(&library, &sel).await.unwrap_err();
        assert!(err.to_string().contains("smart folder"), "msg: {err}");
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_no_unfiled() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Summer Trip")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: BTreeSet::new(),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert_eq!(plan.passes.len(), 2);
        for p in &plan.passes {
            assert!(p.exclude_ids.is_empty());
        }
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_with_unfiled_excludes_member_ids() {
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [folder_record("FOLDER_1", "Vacation")]}))
            .ok(album_count_response(1))
            .ok(asset_page("MASTER_1"))
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: BTreeSet::new(),
            },
            true,
        );

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert_eq!(plan.passes.len(), 2, "1 album pass + 1 unfiled pass");
        assert!(plan.passes[0].exclude_ids.is_empty());
        assert!(
            plan.passes[1].exclude_ids.contains("MASTER_1"),
            "unfiled pass excludes the album's member"
        );
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_respects_excluded_set() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Family")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: names(&["Family"]),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert_eq!(plan.passes.len(), 1, "Family is filtered out");
    }

    #[tokio::test]
    async fn resolve_passes_smart_folder_named_creates_pass() {
        // No user-created folders; smart folders are seeded by
        // `library.albums()` so we don't need a network response for them.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = Selection {
            albums: AlbumSelector::None,
            smart_folders: SmartFolderSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert_eq!(plan.passes[0].album.name.as_ref(), "Favorites");
    }

    #[tokio::test]
    async fn resolve_passes_smart_folder_named_unknown_bails() {
        let mock = MockPhotosSession::new();
        let library = stub_library(mock);
        let sel = Selection {
            albums: AlbumSelector::None,
            smart_folders: SmartFolderSelector::Named {
                included: names(&["NotASmartFolder"]),
                excluded: BTreeSet::new(),
            },
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };

        let err = resolve_passes(&library, &sel).await.unwrap_err();
        assert!(
            err.to_string().contains("not an Apple smart folder"),
            "msg: {err}"
        );
    }

    #[tokio::test]
    async fn resolve_passes_smart_folder_all_sensitive_truth_table() {
        for include_sensitive in [false, true] {
            let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
            let library = stub_library(mock);
            let sel = Selection {
                albums: AlbumSelector::None,
                smart_folders: SmartFolderSelector::All {
                    include_sensitive,
                    excluded: BTreeSet::new(),
                },
                libraries: crate::selection::LibrarySelector::default(),
                unfiled: false,
            };

            let plan = resolve_passes(&library, &sel).await.unwrap();
            let names: BTreeSet<String> = plan
                .passes
                .iter()
                .map(|p| p.album.name.to_string())
                .collect();
            assert!(names.contains("Favorites"), "non-sensitive always present");
            assert_eq!(
                names.contains("Hidden"),
                include_sensitive,
                "Hidden gated on include_sensitive={include_sensitive}"
            );
            assert_eq!(
                names.contains("Recently Deleted"),
                include_sensitive,
                "Recently Deleted gated on include_sensitive={include_sensitive}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_passes_unfiled_with_no_selected_albums_has_empty_exclusion() {
        // None + unfiled=true → library-wide pass, no exclusions.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(AlbumSelector::None, true);

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_excluded_typo_bails() {
        // CG-3: a typo in --album !X must surface the user's mistake. Silent
        // warn lets a real album get included that the user meant to drop.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Family")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: names(&["Vacationn"]),
            },
            false,
        );

        let err = resolve_passes(&library, &sel).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Vacationn") && msg.to_lowercase().contains("not found"),
            "msg: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_passes_named_albums_excluded_typo_bails() {
        // CG-3 paired case: Named selector with a typo'd exclude.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Family")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: names(&["Vacationn"]),
            },
            false,
        );

        let err = resolve_passes(&library, &sel).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Vacationn") && msg.to_lowercase().contains("not found"),
            "msg: {msg}"
        );
    }

    #[tokio::test]
    async fn compute_unfiled_exclude_ids_bails_on_missing_album() {
        // CG-5 defensive: if a selected album is somehow missing from the
        // library album map at exclusion time, surface it instead of
        // silently producing an empty exclusion set (which would let
        // already-filed assets re-download under --unfiled).
        let album_map = std::collections::HashMap::new();
        let selected = vec!["GhostAlbum".to_string()];
        let err = compute_unfiled_exclude_ids(&album_map, &selected)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("GhostAlbum"), "msg: {err}");
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

    // ── init_photos_service wiremock tests ────────────────────────────────
    //
    // These tests use a real `Session` (backed by a temp dir) + a wiremock
    // server to exercise the 421 recovery path without hitting iCloud.
    //
    // Path exercised:
    //   init_photos_service
    //     └─ PhotosService::new
    //          └─ PhotoLibrary::new
    //               └─ retry_post → POST /database/1/.../private/records/query
    //
    // On 421 the function resets the HTTP pool and retries once.  A second 421
    // surfaces MisdirectedRequest to the caller.

    /// Build an `AuthResult` whose `ckdatabasews` URL points at `base_url`.
    /// The `Session` is created against a unique temp dir so concurrent tests
    /// don't fight over the same lock file.
    async fn fake_auth_result(base_url: &str) -> auth::AuthResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = auth::session::Session::new(dir.path(), "test@example.com", base_url, None)
            .await
            .expect("Session::new");
        // Keep the temp dir alive for the session's lifetime.
        // We intentionally leak it here; the OS cleans it up on process exit
        // and these are short-lived tests.
        std::mem::forget(dir);
        let data = auth::responses::AccountLoginResponse {
            ds_info: None,
            webservices: Some(auth::responses::Webservices {
                ckdatabasews: Some(auth::responses::WebserviceEndpoint {
                    url: base_url.to_owned(),
                }),
            }),
            hsa_challenge_required: false,
            hsa_trusted_browser: true,
            domain_to_use: None,
            has_error: false,
            service_errors: vec![],
            i_cdp_enabled: false,
        };
        auth::AuthResult {
            session,
            data,
            requires_2fa: false,
        }
    }

    /// A `RetryConfig` that never sleeps between attempts — keeps tests fast.
    fn no_delay_retry() -> crate::retry::RetryConfig {
        crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        }
    }

    #[tokio::test]
    async fn init_photos_service_recovers_from_421() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        // First call → 421; second call → 200 with a valid CheckIndexingState body.
        let success_body = serde_json::json!({
            "records": [{
                "recordName": "CheckIndexingState",
                "recordType": "CheckIndexingState",
                "fields": {
                    "state": {"value": "FINISHED", "type": "STRING"}
                }
            }]
        });

        Mock::given(method("POST"))
            .and(path_regex(
                r"^/database/1/com\.apple\.photos\.cloud/production/private/records/query",
            ))
            .respond_with(ResponseTemplate::new(421))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(
                r"^/database/1/com\.apple\.photos\.cloud/production/private/records/query",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(&success_body))
            .mount(&mock_server)
            .await;

        let auth_result = fake_auth_result(&mock_server.uri()).await;
        let result = init_photos_service(auth_result, no_delay_retry()).await;

        assert!(
            result.is_ok(),
            "expected recovery from single 421, got: {:?}",
            result.err()
        );

        // Verify the mock received exactly 2 requests (the retry happened).
        let received = mock_server.received_requests().await.expect("requests");
        assert_eq!(
            received.len(),
            2,
            "expected exactly 2 requests (initial 421 + retry), got {}",
            received.len()
        );
    }

    #[tokio::test]
    async fn init_photos_service_fails_on_double_421() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path_regex(
                r"^/database/1/com\.apple\.photos\.cloud/production/private/records/query",
            ))
            .respond_with(ResponseTemplate::new(421))
            .mount(&mock_server)
            .await;

        let auth_result = fake_auth_result(&mock_server.uri()).await;
        let result = init_photos_service(auth_result, no_delay_retry()).await;

        let err = result.expect_err("expected double-421 to return Err");
        // The error must downcast to MisdirectedRequest so sync_loop can
        // route it through the SRP re-auth path.
        let icloud_err = err.downcast_ref::<crate::icloud::error::ICloudError>();
        assert!(
            matches!(
                icloud_err,
                Some(crate::icloud::error::ICloudError::MisdirectedRequest)
            ),
            "expected MisdirectedRequest on double-421, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_passes_named_with_inline_exclude_skips_album() {
        // `--album Vacation --album '!Vacation'` would already bail in the
        // selector parser, but if a programmer constructs that shape
        // directly the resolver must still no-op the included entry.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: names(&["Vacation"]),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel).await.unwrap();
        assert!(plan.passes.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_tags_each_pass_with_correct_kind() {
        // The renderer routes per-category template selection on
        // `pass.kind`, so the resolver must tag each pass correctly. Drift
        // here silently misroutes passes to the wrong template.
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [folder_record("FOLDER_1", "Vacation")]}))
            .ok(album_count_response(0))
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = Selection {
            albums: AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: BTreeSet::new(),
            },
            smart_folders: SmartFolderSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: true,
        };

        let plan = resolve_passes(&library, &sel).await.unwrap();
        let kinds: Vec<(String, PassKind)> = plan
            .passes
            .iter()
            .map(|p| (p.album.name.to_string(), p.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("Vacation".to_string(), PassKind::Album),
                ("Favorites".to_string(), PassKind::SmartFolder),
                (library.all().name.to_string(), PassKind::Unfiled),
            ]
        );
    }

    #[tokio::test]
    async fn resolve_passes_threading_from_cli_input() {
        // End-to-end thread of the v0.13 selection flags through the
        // production parse layer: clap → Cli → Config::build → Selection
        // → resolve_passes. The cli.rs help-shadow tests for these flags
        // only exercise the parser; a regression that drops --smart-folder
        // or flips --unfiled's default between Cli and Selection lands
        // green there. This test is the one place that drives the runtime
        // wiring and asserts on the resolved pass list.
        use crate::cli::{Cli, Command, PasswordArgs};
        use clap::Parser;

        let cookie_dir = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "kei",
            "--username",
            "u@example.com",
            "--data-dir",
            cookie_dir.path().to_str().unwrap(),
            "sync",
            "--album",
            "all",
            "--smart-folder",
            "Favorites",
            "--unfiled",
            "false",
            "--library",
            "shared",
        ])
        .unwrap();
        let Command::Sync { sync, .. } = cli.effective_command() else {
            panic!("expected Sync subcommand");
        };
        let globals = crate::config::GlobalArgs::from_cli(&cli);
        let cfg =
            crate::config::Config::build(&globals, &PasswordArgs::default(), sync, None).unwrap();

        // Sanity-check the cli → Selection wiring before pinning the pass list.
        assert_eq!(cfg.selection.libraries.to_raw(), vec!["shared".to_string()]);
        assert!(matches!(
            cfg.selection.albums,
            AlbumSelector::All { ref excluded } if excluded.is_empty()
        ));
        assert!(matches!(
            cfg.selection.smart_folders,
            SmartFolderSelector::Named { ref included, ref excluded }
                if included.contains("Favorites") && excluded.is_empty()
        ));
        assert!(!cfg.selection.unfiled);

        // Stub a library exposing one user album ("Vacation") plus the
        // Favorites smart folder. resolve_passes only consumes the first
        // response (`library.albums().await?`); subsequent .ok()s match
        // the resolve_passes_tags_each_pass_with_correct_kind pattern.
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [folder_record("FOLDER_VAC", "Vacation")]}))
            .ok(album_count_response(0))
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);

        let plan = resolve_passes(&library, &cfg.selection).await.unwrap();
        let pairs: Vec<(String, PassKind)> = plan
            .passes
            .iter()
            .map(|p| (p.album.name.to_string(), p.kind))
            .collect();
        // --album all → 1 Album pass, --smart-folder Favorites → 1 SmartFolder
        // pass, --unfiled false → 0 Unfiled passes.
        assert_eq!(
            pairs,
            vec![
                ("Vacation".to_string(), PassKind::Album),
                ("Favorites".to_string(), PassKind::SmartFolder),
            ],
            "expected [Album(Vacation), SmartFolder(Favorites)] (no unfiled), got {pairs:?}"
        );
    }

    // ── resolve_libraries tests ──────────────────────────────────────

    /// Build a `PhotosService` with a stub primary library plus the named
    /// shared zones. Bypasses CloudKit listing endpoints by pre-populating
    /// the lazy library maps.
    fn photos_service_with_zones(shared_zones: &[&str]) -> icloud::photos::PhotosService {
        use crate::icloud::photos::PhotoLibrary;
        use std::collections::HashMap;

        let primary =
            PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), "PrimarySync");
        let mut shared = HashMap::new();
        for zone in shared_zones {
            shared.insert(
                (*zone).to_string(),
                PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), zone),
            );
        }
        icloud::photos::PhotosService::for_testing_with_libraries(
            Box::new(MockPhotosSession::new()),
            primary,
            HashMap::new(),
            shared,
        )
    }

    fn selector_from(raw: &[&str]) -> crate::selection::LibrarySelector {
        let owned: Vec<String> = raw.iter().map(|s| (*s).to_string()).collect();
        crate::selection::parse_library_selector(&owned).unwrap()
    }

    fn zone_names(libs: &[icloud::photos::PhotoLibrary]) -> Vec<&str> {
        libs.iter()
            .map(icloud::photos::PhotoLibrary::zone_name)
            .collect()
    }

    #[tokio::test]
    async fn resolve_libraries_default_returns_primary_only() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = crate::selection::LibrarySelector::default();
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(zone_names(&libs), vec!["PrimarySync"]);
    }

    #[tokio::test]
    async fn resolve_libraries_all_returns_every_zone() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["all"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["PrimarySync", "SharedSync-AAAA1111", "SharedSync-BBBB2222"]
        );
    }

    #[tokio::test]
    async fn resolve_libraries_shared_only_excludes_primary() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["shared"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
    }

    #[tokio::test]
    async fn resolve_libraries_named_zone_returns_only_that_zone() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(zone_names(&libs), vec!["SharedSync-AAAA1111"]);
    }

    #[tokio::test]
    async fn resolve_libraries_named_zone_matches_truncated_form() {
        let mut ps =
            photos_service_with_zones(&["SharedSync-AAAA1111-2222-3333-4444-555555555555"]);
        let sel = selector_from(&["SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(
            zone_names(&libs),
            vec!["SharedSync-AAAA1111-2222-3333-4444-555555555555"]
        );
    }

    #[tokio::test]
    async fn resolve_libraries_primary_plus_named_returns_both() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["primary", "SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["PrimarySync", "SharedSync-AAAA1111"]);
    }

    #[tokio::test]
    async fn resolve_libraries_multiple_named_zones() {
        let mut ps = photos_service_with_zones(&[
            "SharedSync-AAAA1111",
            "SharedSync-BBBB2222",
            "SharedSync-CCCC3333",
        ]);
        let sel = selector_from(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
    }

    #[tokio::test]
    async fn resolve_libraries_truncated_collision_bails() {
        // CloudKit UUIDs are not guaranteed unique on the leading 8 hex
        // characters that `truncate_library_zone` keeps. Two distinct
        // shared zones whose UUIDs collide on that prefix render to the
        // same `{library}` segment on disk; without a resolver-time bail,
        // multi-library sync silently overwrites one zone's bytes with
        // the other's. State DB stays per-zone-distinct under the v8 PK
        // so the corruption is invisible until users notice missing
        // files. The repo's own paths.rs unit test pins the prefix
        // collision invariant; this test pins the bail that relies on it.
        let mut ps = photos_service_with_zones(&[
            "SharedSync-AAAA1111-EEEE-2222-3333-444444444444",
            "SharedSync-AAAA1111-FFFF-5555-6666-777777777777",
        ]);
        let sel = selector_from(&["all"]);
        let err = resolve_libraries(&sel, &mut ps)
            .await
            .expect_err("two zones sharing the 8-hex truncation prefix must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("SharedSync-AAAA1111-EEEE-2222-3333-444444444444")
                && msg.contains("SharedSync-AAAA1111-FFFF-5555-6666-777777777777"),
            "bail must name both colliding full UUIDs so the user can pin a longer form; got: {msg}"
        );
        assert!(
            msg.contains("SharedSync-AAAA1111"),
            "bail must include the truncated form that's the collision point; got: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_libraries_truncated_collision_with_explicit_zone_does_not_bail() {
        // The bail is only relevant when *both* colliding zones land in
        // the chosen set. If the user pins one of them via a longer
        // `--library SharedSync-AAAA1111-EEEE`, the other is excluded
        // and the on-disk paths can't collide, so the resolver must not
        // bail.
        let mut ps = photos_service_with_zones(&[
            "SharedSync-AAAA1111-EEEE-2222-3333-444444444444",
            "SharedSync-AAAA1111-FFFF-5555-6666-777777777777",
        ]);
        let sel = selector_from(&["SharedSync-AAAA1111-EEEE-2222-3333-444444444444"]);
        let libs = resolve_libraries(&sel, &mut ps)
            .await
            .expect("explicit single-zone selection must not trip the collision bail");
        assert_eq!(
            zone_names(&libs),
            vec!["SharedSync-AAAA1111-EEEE-2222-3333-444444444444"]
        );
    }

    #[tokio::test]
    async fn resolve_libraries_exclusion_drops_named_zone() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["all", "!SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["PrimarySync", "SharedSync-BBBB2222"]);
    }

    #[tokio::test]
    async fn resolve_libraries_exclusion_via_shared_sentinel() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["all", "!shared"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(zone_names(&libs), vec!["PrimarySync"]);
    }

    #[tokio::test]
    async fn resolve_libraries_named_miss_bails_with_helpful_error() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111"]);
        let sel = selector_from(&["SharedSync-NOPE"]);
        let err = resolve_libraries(&sel, &mut ps).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SharedSync-NOPE"),
            "miss must name the input: {msg}"
        );
        assert!(
            msg.contains("Available zones") || msg.contains("kei list libraries"),
            "miss must point at discovery: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_libraries_zero_match_after_exclusion_bails() {
        let mut ps = photos_service_with_zones(&[]);
        let sel = selector_from(&["primary", "!primary"]);
        let err = resolve_libraries(&sel, &mut ps).await.unwrap_err();
        assert!(
            err.to_string().contains("zero libraries"),
            "unexpected error: {err}"
        );
    }
}
